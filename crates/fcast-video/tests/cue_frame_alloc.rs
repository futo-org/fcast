//! What one displayed frame costs the cue engine, counted rather than argued.
//!
//! The engine's own gate (`cue_overlay`'s `alloc_gate` over in `receiver-ui`)
//! shows a single cue with NOTHING QUEUED BEHIND IT, which is the one shape
//! that skips `resolve_work`'s boundary warm entirely: the warm bails at
//! `pending.front()?`. Every real subtitle file has thousands of cues pending,
//! so the arm the gate never reached is the arm that runs on every frame in the
//! field. These fixtures keep a queue behind the cue on purpose.
//!
//! Own binary because `#[global_allocator]` is process-wide. The counter is per
//! thread so the engine's layout worker, which allocates freely and by design,
//! cannot be mistaken for the frame path.

use std::{
    alloc::{GlobalAlloc, Layout, System},
    cell::Cell,
    time::{Duration, Instant},
};

use fcast_video::cue::{CueEngine, CueInput, TextFormat};

thread_local! {
    static COUNT: Cell<u64> = const { Cell::new(0) };
}

struct Counting;

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        COUNT.with(|c| c.set(c.get() + 1));
        unsafe { System.alloc(layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        COUNT.with(|c| c.set(c.get() + 1));
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new: usize) -> *mut u8 {
        COUNT.with(|c| c.set(c.get() + 1));
        unsafe { System.realloc(ptr, layout, new) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// Allocations `f` makes on this thread.
fn measure<T>(f: impl FnOnce() -> T) -> (T, u64) {
    let before = COUNT.with(|c| c.get());
    let out = f();
    let after = COUNT.with(|c| c.get());
    (out, after - before)
}

const CANVAS: (u32, u32) = (1920, 1080);
/// The instant every fixture below is measured at.
const AT: u64 = 1;

fn text_cue(text: &str, start_s: u64, end_s: u64) -> CueInput {
    CueInput {
        format: TextFormat::Utf8,
        text: text.to_owned(),
        start_rt: gst::ClockTime::from_seconds(start_s),
        end_rt: Some(gst::ClockTime::from_seconds(end_s)),
    }
}

/// A warm scene-lane engine showing `wanted` cues at [`AT`], with `queued` more
/// still pending behind them. Layout runs on the engine's own worker on every
/// lane, so the settle is a wait rather than a call.
fn warm(showing: &[CueInput], queued: usize) -> CueEngine {
    gst::init().unwrap();
    let engine = CueEngine::for_scene_consumer();
    engine.set_canvas(CANVAS.0, CANVAS.1);
    for cue in showing {
        engine.submit(cue.clone());
    }
    // Far enough out that none of them ever becomes due during a measurement,
    // and distinct text so they cannot collapse into one key.
    for i in 0..queued {
        let at = 600 + i as u64 * 10;
        engine.submit(text_cue(&format!("queued line number {i}"), at, at + 5));
    }
    let at = gst::ClockTime::from_seconds(AT);
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if engine.scenes_for(Some(at)).len() == showing.len() {
            break;
        }
        assert!(Instant::now() < deadline, "the cue scenes never arrived");
        std::thread::sleep(Duration::from_millis(10));
    }
    // The boundary warm builds the NEXT cue too, and that build is a real job
    // on the worker. Let it finish, otherwise the measurement below catches the
    // engine still asking for it rather than the settled state.
    let settle = Instant::now() + Duration::from_secs(30);
    while Instant::now() < settle {
        engine.scenes_for(Some(at));
        std::thread::sleep(Duration::from_millis(10));
        let (_, n) = measure(|| engine.scenes_for(Some(at)));
        if n == 0 {
            break;
        }
    }
    engine
}

/// Eight frames of the settled engine, and what each cost.
fn per_frame(engine: &CueEngine) -> [u64; 8] {
    let at = gst::ClockTime::from_seconds(AT);
    let mut counts = [0u64; 8];
    for c in counts.iter_mut() {
        let (_, n) = measure(|| {
            let shown = engine.scenes_for(Some(at));
            std::hint::black_box(&shown);
        });
        *c = n;
    }
    counts
}

/// THE REGRESSION THIS FILE EXISTS FOR.
///
/// A cue on screen with a queue behind it, which is every frame of every real
/// subtitle track. The boundary warm used to build a whole `SceneKey` here on
/// every frame, cloning the next cue's text onto the heap only to compare it
/// and drop it.
#[test]
fn a_steady_cue_with_a_queue_behind_it_costs_the_frame_nothing() {
    let engine = warm(&[text_cue("a steady subtitle line", 0, 10)], 200);
    let counts = per_frame(&engine);
    eprintln!("cue with a queue behind it, allocations per frame: {counts:?}");
    assert_eq!(
        counts, [0u64; 8],
        "the frame path allocates with cues still pending, which is the state \
         every real file is in for its whole duration"
    );
}

/// The same claim on the lane the desktop wgpu receiver actually runs: the
/// paint lane off. Turning it off must not leave the boundary warm's key build
/// behind, which is exactly what it used to do.
#[test]
fn the_paint_lane_off_does_not_leave_a_warm_probe_allocating() {
    let engine = warm(&[text_cue("scene consumer line", 0, 10)], 200);
    assert_eq!(engine.cached_pixels(), 0, "the scene lane painted a raster");
    let counts = per_frame(&engine);
    eprintln!("scene consumer, allocations per frame: {counts:?}");
    assert_eq!(counts, [0u64; 8], "the scene lane allocates per frame");
}

/// A FULL STACK, which is where the shown set used to spill off the stack.
///
/// `scenes_for` returned a `SmallVec` with two inline slots against a cap of
/// eight active cues, so a third simultaneous cue put both it and the placement
/// scratch on the heap on every frame.
#[test]
fn a_full_stack_of_cues_never_spills_the_shown_set() {
    let cues: Vec<_> = (0..8)
        .map(|i| text_cue(&format!("stacked line {i}"), 0, 10))
        .collect();
    let engine = warm(&cues, 200);
    let at = gst::ClockTime::from_seconds(AT);
    assert_eq!(
        engine.scenes_for(Some(at)).len(),
        8,
        "the whole stack must be showing for this to prove anything"
    );
    let counts = per_frame(&engine);
    eprintln!("eight stacked cues, allocations per frame: {counts:?}");
    assert_eq!(
        counts, [0u64; 8],
        "a stack past the inline capacity allocates per frame"
    );
}

/// Nothing loaded at all. The cheapest state there is, and the one a paused
/// player with no subtitle track sits in.
#[test]
fn an_idle_engine_costs_the_frame_nothing() {
    gst::init().unwrap();
    let engine = CueEngine::for_scene_consumer();
    engine.set_canvas(CANVAS.0, CANVAS.1);
    for _ in 0..4 {
        engine.scenes_for(Some(gst::ClockTime::from_seconds(AT)));
    }
    let counts = per_frame(&engine);
    eprintln!("idle engine, allocations per frame: {counts:?}");
    assert_eq!(counts, [0u64; 8], "an engine with no cues allocates");
}

/// The bitmap read, which the desktop lane calls beside the scene read on every
/// frame. Documented as a borrow with no `Arc` traffic; graded here.
#[test]
fn reading_the_shown_bitmaps_costs_the_frame_nothing() {
    let engine = warm(&[text_cue("a line beside a subpicture", 0, 10)], 200);
    let at = gst::ClockTime::from_seconds(AT);
    for _ in 0..4 {
        engine.scenes_for(Some(at));
        engine.with_shown_bitmaps(|regions| regions.len());
    }
    let mut counts = [0u64; 8];
    for c in counts.iter_mut() {
        let (_, n) = measure(|| {
            engine.scenes_for(Some(at));
            engine.with_shown_bitmaps(|regions| regions.len())
        });
        *c = n;
    }
    eprintln!("scene plus bitmap read, allocations per frame: {counts:?}");
    assert_eq!(counts, [0u64; 8], "the paired per frame reads allocate");
}
