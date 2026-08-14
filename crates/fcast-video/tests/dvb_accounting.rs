//! Verifies the decoder's allocation accounting against a counting global
//! allocator. Self-reported numbers alone cannot catch heap the decoder
//! forgets to charge itself for.
//!
//! Own binary because `#[global_allocator]` is process-wide. The lock keeps
//! the two tests from measuring each other. Harness noise remains, so
//! assertions are bounds, not equalities.

use std::{
    alloc::{GlobalAlloc, Layout, System},
    sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use fcast_video::subpic::{
    BitmapFormat, BitmapPacket, SubpicDecoder,
    dvb::{DvbDecoder, fixtures},
    pgs::ALLOCATION_BUDGET,
};

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

struct Counting;

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc(layout) };
        if !pointer.is_null() {
            let now = LIVE.fetch_add(layout.size(), Ordering::Relaxed) + layout.size();
            PEAK.fetch_max(now, Ordering::Relaxed);
        }
        pointer
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
        unsafe { System.dealloc(pointer, layout) }
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let moved = unsafe { System.realloc(pointer, layout, new_size) };
        if !moved.is_null() {
            if new_size >= layout.size() {
                let grew = new_size - layout.size();
                let now = LIVE.fetch_add(grew, Ordering::Relaxed) + grew;
                PEAK.fetch_max(now, Ordering::Relaxed);
            } else {
                LIVE.fetch_sub(layout.size() - new_size, Ordering::Relaxed);
            }
        }
        moved
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// One measurement at a time.
static MEASURING: Mutex<()> = Mutex::new(());

fn live() -> usize {
    LIVE.load(Ordering::Relaxed)
}

fn reset_peak() {
    PEAK.store(LIVE.load(Ordering::Relaxed), Ordering::Relaxed);
}

fn peak() -> usize {
    PEAK.load(Ordering::Relaxed)
}

fn packet(bytes: &[u8], rt_ms: u64) -> BitmapPacket {
    BitmapPacket {
        format: BitmapFormat::Dvb,
        data: gst::Buffer::from_slice(bytes.to_vec()),
        codec_data: None,
        rt: gst::ClockTime::from_mseconds(rt_ms),
        duration: None,
    }
}

/// What the decoder says it holds covers what the allocator gave it, through
/// the path where a stream grows a list (object placements) without growing a
/// picture.
#[test]
fn the_charge_covers_the_real_heap() {
    // Recover a lock poisoned by the other test's failure so one failure is
    // not reported as two.
    let _measuring = MEASURING
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    gst::init().expect("gst init");
    let mut decoder = DvbDecoder::new();
    decoder.set_video_size(1920, 1080);

    let objects: Vec<(u16, u16, u16)> = (0..2_000u32).map(|index| (index as u16, 0, 0)).collect();
    let before = live();
    for id in 0..64u8 {
        let bytes = fixtures::data_field(&[fixtures::region(id, 64, 64, 4, 0, 0, true, &objects)]);
        decoder.push(&packet(&bytes, 0));
    }
    let real = live().saturating_sub(before);
    let held = decoder.held_bytes() as usize;
    let allocated = decoder.allocated_bytes() as usize;

    // The decoder is not the only allocator in this window (buffer pools, the
    // harness). The slack covers that noise and is orders of magnitude below
    // the uncounted-heap defect this exists to catch.
    const SLACK: usize = 256 * 1024;
    assert!(
        held + SLACK >= real,
        "the allocator handed over {real} bytes and the decoder charged itself {held}"
    );
    assert!(
        allocated <= held,
        "the measurement {allocated} is above the charge {held}"
    );
    // A wildly inflated charge would satisfy the bound above and mean nothing.
    assert!(
        held < real * 4 + 1_048_576,
        "the charge {held} is wildly larger than the {real} bytes really held"
    );
    assert!(
        (held as u64) <= ALLOCATION_BUDGET,
        "the cap did not hold: {held}"
    );
}

/// A resize never holds the old buffer and the new one at once.
///
/// `region.pixels = vec![..]` evaluates the right-hand side before dropping
/// the left, so a naive resize peaks at twice the budget. Only an allocator
/// can see a peak.
#[test]
fn a_resize_never_holds_both_buffers() {
    // Recover a lock poisoned by the other test's failure so one failure is
    // not reported as two.
    let _measuring = MEASURING
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    gst::init().expect("gst init");
    let mut decoder = DvbDecoder::new();
    decoder.set_video_size(1920, 1080);

    // 8192x4000 indices is 31.25 MiB, as much as fits in the budget after the
    // hash tables' own reserve.
    const SIDE: u16 = 8192;
    const OTHER: u16 = 4000;
    const BYTES: u64 = SIDE as u64 * OTHER as u64;
    let first = fixtures::data_field(&[fixtures::region(1, SIDE, OTHER, 4, 0, 0, true, &[])]);
    decoder.push(&packet(&first, 0));
    let held = decoder.held_bytes();
    assert!(held >= BYTES, "the big region was refused: {held}");

    reset_peak();
    let base = live();
    // Same byte count, different rectangle, exercising the resize branch.
    let second = fixtures::data_field(&[fixtures::region(1, OTHER, SIDE, 4, 0, 0, true, &[])]);
    decoder.push(&packet(&second, 1_000));
    let over_base = peak().saturating_sub(base);

    assert!(
        (over_base as u64) < ALLOCATION_BUDGET / 2,
        "the resize peaked {over_base} bytes above its base, which is both buffers at once"
    );
    assert!(decoder.held_bytes() >= BYTES, "the resized region is gone");
}
