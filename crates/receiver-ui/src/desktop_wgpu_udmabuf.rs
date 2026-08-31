//! udmabuf pools for the software-decode arm of the desktop wgpu video lane.
//!
//! A VA-API decoder hands its frames over as dmabuf fds because the appsink
//! offers `memory:DMABuf` caps and the decoder allocates the surfaces itself.
//! A software decoder cannot: it writes with the CPU, so it negotiates plain
//! `video/x-raw` and the lane uploads every picture, which at 4K 10-bit is
//! ~24MB of `write_texture` per frame.
//!
//! The way out is the memory, not the caps. `memfd_create` plus
//! `UDMABUF_CREATE` turns ordinary pages into a dma_buf: the CPU writes them
//! like any other mapping, and the GPU imports them like any other dmabuf.
//! So the lane answers the decoder's ALLOCATION query with a buffer pool
//! backed by that allocator, the decoder writes its pixels straight into
//! pages the GPU can read, and [`crate::desktop_wgpu_dmabuf`] imports them
//! with the linear modifier. No copy on either side. It is the same trick
//! GTK4 uses for software frames.
//!
//! # Whose allocator
//!
//! Both halves are gst's own, since 1.28: `GstUdmabufAllocator` does the
//! memfd/ioctl dance and `GstVideoDmabufPool` wraps it with the video
//! alignment every decoder wants (and, where the kernel exports sync files,
//! defers recycling a buffer until the GPU's read of it has retired). Our
//! static tree is 1.29, so nothing here reimplements either; this module is
//! the policy that decides when to offer them.
//!
//! # Degradation
//!
//! `/dev/udmabuf` is `root:kvm 0660` on most distributions, so a plain user
//! account often cannot open it. That is not an error: [`available`] probes
//! once, says so once, and every later query is answered with nothing at all,
//! which leaves the decoder allocating its own system memory exactly as it
//! does today. A pool that refuses its own config mid-session latches the
//! proposals off for the rest of the process for the same reason.
//!
//! `FCAST_DESKTOP_WGPU_UDMABUF=0` turns the arm off by hand, which is the A/B
//! control the measurement test uses.

use gst::prelude::*;
use gst_video::prelude::*;
use i_slint_video_wgpu::PixelFormat;
use std::sync::{
    OnceLock,
    atomic::{AtomicBool, Ordering},
};
use tracing::{info, warn};

/// The stride and address alignment the pool wants, as a gst alignment MASK
/// (so 256 bytes). `GstVideoDmabufPool` forces this value itself, on the
/// grounds that several AMD GPUs need it to import a linear buffer at all;
/// matching it here means our config is accepted on the first try instead of
/// coming back "updated" for a second round.
const ALIGN_MASK: u32 = 255;

/// Buffers asked of the pool up front. The appsink holds two, the render
/// borrows three more while the GPU reads them, and one is in flight, so
/// anything under six makes the decoder grow the pool on its first pass.
/// `max` is left at zero (unbounded) so a decoder with a deep DPB is never
/// blocked waiting on a slot the lane is holding.
const MIN_BUFFERS: u32 = 6;

/// Set when the pool or the allocator failed at runtime. Nothing is retried
/// after that: the failure is a device-level refusal (a revoked `/dev/udmabuf`,
/// an ioctl that started returning ENOMEM), not something the next stream
/// fixes, and the upload arm carries everything meanwhile.
static REFUSED: AtomicBool = AtomicBool::new(false);

/// `FCAST_DESKTOP_WGPU_UDMABUF=0` leaves the appsink proposing nothing, so
/// every software frame takes the upload arm. Read per call, not cached, so a
/// test can flip it.
pub fn enabled() -> bool {
    !std::env::var("FCAST_DESKTOP_WGPU_UDMABUF").is_ok_and(|v| v == "0")
}

/// Whether this box can hand out udmabufs at all.
///
/// Two things have to hold and both are probed once, here: gst registered the
/// allocator (which it only does when `/dev/udmabuf` opened), and one real
/// allocation went through (which is what an EPERM on `UDMABUF_CREATE` shows,
/// since opening the device and using it are separately permitted).
///
/// Probing costs one memfd and one ioctl for the life of the process. Doing it
/// lazily per query instead would put a failing syscall pair on every caps
/// negotiation of a box that can never satisfy them.
pub fn available() -> bool {
    static OK: OnceLock<bool> = OnceLock::new();
    *OK.get_or_init(|| {
        if gst::init().is_err() {
            return false;
        }
        let Some(allocator) = gst_allocators::UdmabufAllocator::get() else {
            info!(
                "wgpu video lane: no udmabuf allocator, /dev/udmabuf is unreadable, \
                 software frames take the upload arm"
            );
            return false;
        };
        // one page, freed immediately: proof the ioctl is permitted, not only
        // the open
        match allocator.alloc(4096, None) {
            Ok(_) => true,
            Err(err) => {
                info!(
                    %err,
                    "wgpu video lane: udmabuf allocation refused, software frames take \
                     the upload arm"
                );
                false
            }
        }
    })
}

/// Records a runtime refusal and stops every later proposal.
fn refuse(reason: &str) {
    if !REFUSED.swap(true, Ordering::Relaxed) {
        warn!(
            reason,
            "wgpu video lane: udmabuf pool refused, no longer proposing one"
        );
    }
}

/// Whether a buffer's memories are all dmabufs.
///
/// This is the whole detection on the software arm: a udmabuf pool's buffers
/// arrive under ordinary `video/x-raw` caps, so the caps feature says nothing
/// and only the memory type does. One downcast per memory, no map, no
/// allocation, which is what makes it cheap enough to ask per frame.
pub fn is_dmabuf(buffer: &gst::BufferRef) -> bool {
    let n = buffer.n_memory();
    n > 0
        && (0..n).all(|i| {
            buffer
                .peek_memory(i)
                .downcast_memory_ref::<gst_allocators::DmaBufMemory>()
                .is_some()
        })
}

/// Answers a system-memory ALLOCATION query with a udmabuf-backed video pool.
///
/// `formats` is what the lane can import through the LINEAR modifier, which is
/// the only layout a udmabuf can ever have: the pages are plain memory and
/// nothing tiles them. A format outside that list is left alone rather than
/// proposed and then refused at import time.
///
/// `DMA_DRM` caps are never touched. A VA decoder allocates its own surfaces
/// and the import path already takes them zero-copy, so a pool here would only
/// compete with it.
///
/// Returns whether the query gained a pool. The decoder is free to ignore it,
/// and then nothing changes. The ones that matter do take it: libav's software
/// decoders through `gst_video_decoder`'s own allocation, and dav1ddec through
/// its `decide_allocation`, which hands a pool that carries the video meta and
/// the video alignment straight to dav1d's picture allocator (proved on 8 and
/// 10 bit AV1 in `desktop_wgpu_video`'s tests).
pub fn propose(query: &mut gst::query::Allocation, formats: &[PixelFormat]) -> bool {
    if formats.is_empty() || !enabled() || REFUSED.load(Ordering::Relaxed) || !available() {
        return false;
    }
    // owned, because the query is written to below and the caps borrow it
    let (Some(caps), need_pool) = query.get_owned() else {
        return false;
    };
    if !need_pool || gst_video::is_dma_drm_caps(&caps) {
        return false;
    }
    let Ok(mut info) = gst_video::VideoInfo::from_caps(&caps) else {
        return false;
    };
    let Some(format) = crate::desktop_wgpu_video::map_format(info.format()) else {
        return false;
    };
    if !formats.contains(&format) {
        return false;
    }

    // The decoder's own padding rides on top of this, so the pool is sized
    // from the aligned info rather than the caps' packed one. Getting it wrong
    // low means the first acquire re-negotiates; getting it wrong high wastes
    // a page a buffer.
    let mut align =
        gst_video::VideoAlignment::new(0, 0, 0, 0, &[ALIGN_MASK; gst_video::VIDEO_MAX_PLANES]);
    if info.align(&mut align).is_err() {
        return false;
    }
    let size = info.size() as u32;

    let Some(pool) = gst_video::VideoDmabufPool::new() else {
        refuse("the build has no video dmabuf pool");
        return false;
    };
    let Some(allocator) = gst_allocators::UdmabufAllocator::get() else {
        refuse("the udmabuf allocator went away");
        return false;
    };
    let mut params = gst::AllocationParams::default();
    params.set_align(ALIGN_MASK as usize);

    let mut config = pool.config();
    config.set_params(Some(&caps), size, MIN_BUFFERS, 0);
    // Both options are mandatory for this pool: it refuses a config without
    // them. They are also what the import needs, since a dmabuf's plane
    // offsets and pitches live in the video meta and nowhere else.
    config.add_option(gst_video::BUFFER_POOL_OPTION_VIDEO_META);
    config.add_option(gst_video::BUFFER_POOL_OPTION_VIDEO_ALIGNMENT);
    config.set_video_alignment(&align);
    config.set_allocator(Some(&allocator), Some(&params));
    if pool.set_config(config).is_err() {
        refuse("the pool would not take the lane's config");
        return false;
    }

    query.add_allocation_pool(Some(&pool), size, MIN_BUFFERS, 0);
    query.add_allocation_param(Some(&allocator), params);
    if query
        .find_allocation_meta::<gst_video::VideoMeta>()
        .is_none()
    {
        query.add_allocation_meta::<gst_video::VideoMeta>(None);
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    fn nv12_caps(width: u32, height: u32) -> gst::Caps {
        gst::Caps::from_str(&format!(
            "video/x-raw, format=(string)NV12, width=(int){width}, height=(int){height}, \
             framerate=(fraction)30/1"
        ))
        .unwrap()
    }

    fn allocation(caps: &gst::Caps, need_pool: bool) -> gst::query::Allocation<gst::Query> {
        gst::query::Allocation::new(Some(caps), need_pool)
    }

    /// The box this runs on either has the device or it does not, and the
    /// answer must be the same every time it is asked. Everything else here
    /// skips when it is false, which is the permission fallback in practice.
    #[test]
    fn availability_is_stable_and_says_which_way_it_went() {
        gst::init().unwrap();
        let first = available();
        assert_eq!(first, available(), "the probe must be latched");
        eprintln!("udmabuf available: {first}");
    }

    /// The proposal itself: a pool lands on the query, it is the dmabuf pool,
    /// and the buffers it hands out really are dmabufs carrying a video meta
    /// at the aligned pitch. That last part is what the import reads.
    #[test]
    fn a_sysmem_query_gains_a_pool_whose_buffers_are_dmabufs() {
        gst::init().unwrap();
        if !enabled() || !available() {
            eprintln!("no udmabuf on this box, skipping");
            return;
        }
        let caps = nv12_caps(1920, 1080);
        let mut query = allocation(&caps, true);
        assert!(propose(&mut query, &[PixelFormat::Nv12]), "must propose");

        assert!(
            query
                .find_allocation_meta::<gst_video::VideoMeta>()
                .is_some(),
            "the pool is useless without the meta"
        );
        let (pool, size, min, max) = query
            .allocation_pools()
            .next()
            .expect("a pool must be there");
        let pool = pool.expect("the pool must not be the null placeholder");
        assert!(
            pool.is::<gst_video::VideoDmabufPool>(),
            "the proposal must be the dmabuf pool"
        );
        assert!(size >= 1920 * 1080 * 3 / 2, "size {size} is under a frame");
        assert_eq!(min, MIN_BUFFERS);
        assert_eq!(max, 0, "an unbounded pool cannot starve the decoder");

        pool.set_active(true).expect("the pool must start");
        let buffer = pool.acquire_buffer(None).expect("a buffer must come out");
        assert!(is_dmabuf(&buffer), "the buffer must be dmabuf backed");
        let meta = buffer
            .meta::<gst_video::VideoMeta>()
            .expect("the buffer must carry a video meta");
        for stride in &meta.stride()[..meta.n_planes() as usize] {
            assert_eq!(
                *stride as u32 & ALIGN_MASK,
                0,
                "plane pitch {stride} is not 256 aligned"
            );
        }
        drop(buffer);
        pool.set_active(false).unwrap();
    }

    /// The VA path is untouched. `DMA_DRM` caps mean the decoder allocates its
    /// own surfaces, and a pool here would only fight it for them.
    #[test]
    fn dma_drm_caps_are_never_proposed_for() {
        gst::init().unwrap();
        let caps = gst::Caps::from_str(
            "video/x-raw(memory:DMABuf), format=(string)DMA_DRM, drm-format=(string)NV12, \
             width=(int)1920, height=(int)1080, framerate=(fraction)30/1",
        )
        .unwrap();
        let mut query = allocation(&caps, true);
        assert!(!propose(&mut query, &[PixelFormat::Nv12]));
        assert_eq!(query.allocation_pools().count(), 0);
    }

    /// A format the device cannot sample through a linear dmabuf, a query that
    /// did not ask for a pool, and caps the lane cannot map at all: three
    /// separate reasons to leave the query alone rather than propose something
    /// the import would then refuse.
    #[test]
    fn nothing_is_proposed_outside_the_importable_set() {
        gst::init().unwrap();
        let caps = nv12_caps(1920, 1080);

        let mut query = allocation(&caps, true);
        assert!(
            !propose(&mut query, &[]),
            "an empty import set proposes nothing"
        );

        let mut query = allocation(&caps, true);
        assert!(
            !propose(&mut query, &[PixelFormat::I420]),
            "a format the device cannot import must not be proposed for"
        );

        let mut query = allocation(&caps, false);
        assert!(
            !propose(&mut query, &[PixelFormat::Nv12]),
            "a query that wants no pool must get none"
        );

        let rgb = gst::Caps::from_str(
            "video/x-raw, format=(string)RGB, width=(int)64, height=(int)64, \
             framerate=(fraction)30/1",
        )
        .unwrap();
        let mut query = allocation(&rgb, true);
        assert!(
            !propose(&mut query, &[PixelFormat::Nv12]),
            "caps the lane has no render path for must not be proposed for"
        );
    }

    /// A plain buffer is not mistaken for a pool one. The detection is the
    /// only thing standing between a sysmem frame and an import that would
    /// refuse it, and it runs on every software frame.
    #[test]
    fn a_plain_system_memory_buffer_is_not_a_dmabuf() {
        gst::init().unwrap();
        let buffer = gst::Buffer::with_size(4096).unwrap();
        assert!(!is_dmabuf(&buffer));
        assert!(
            !is_dmabuf(&gst::Buffer::new()),
            "an empty buffer has nothing to import"
        );
    }
}
