//! Idle retirement of the engine's two worker threads, proved against the
//! operating system rather than the engine's own bookkeeping.
//!
//! `fvid-cue-raster` and `fvid-sub-decode` are lazily spawned and lazily
//! unspawned. An idle worker ends its thread and the next piece of work
//! spawns a fresh one. Otherwise a long-lived receiver accumulates parked
//! worker threads.
//!
//! Own binary because the honest assertion counts threads by name in
//! `/proc/self/task`, a process-wide question that other tests' engines
//! would pollute. Here the two tests use different worker names.
//! `workers_live()` is asserted beside the count because that is what
//! submitters consult.

use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use fcast_video::{
    cue::{CueEngine, CueInput, TextFormat},
    subpic::{BitmapFormat, BitmapPacket, BitmapRegion, DisplayUpdate, SubpicDecoder},
};

/// Short enough to watch, long enough that a loaded machine does not retire a
/// worker between two statements of the same test.
const IDLE: Duration = Duration::from_millis(50);

fn wait_for(what: &str, condition: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        if condition() {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out waiting for {what}");
}

/// How many live threads of this process carry `name`.
///
/// Linux only. Elsewhere this answers zero and its assertions are skipped.
/// `workers_live()` is still asserted on every platform.
#[cfg(target_os = "linux")]
fn threads_named(name: &str) -> usize {
    let Ok(entries) = std::fs::read_dir("/proc/self/task") else {
        return 0;
    };
    entries
        .filter_map(|entry| entry.ok())
        .filter(|entry| {
            std::fs::read_to_string(entry.path().join("comm")).is_ok_and(|comm| comm.trim() == name)
        })
        .count()
}

#[cfg(not(target_os = "linux"))]
fn threads_named(_name: &str) -> usize {
    0
}

/// A formatless decoder. One 2x2 region carrying the packet's first byte,
/// enough to see that a decoder ran.
struct TagDecoder {
    pushes: Arc<AtomicU64>,
}

impl SubpicDecoder for TagDecoder {
    fn set_codec_data(&mut self, _data: &[u8]) {}
    fn set_video_size(&mut self, _width: u32, _height: u32) {}
    fn reset(&mut self) {}

    fn push(&mut self, packet: &BitmapPacket) -> Vec<DisplayUpdate> {
        self.pushes.fetch_add(1, Ordering::Relaxed);
        let tag = packet
            .data
            .map_readable()
            .ok()
            .and_then(|map| map.first().copied())
            .unwrap_or(0);
        vec![DisplayUpdate {
            start_rt: packet.rt,
            end_rt: None,
            regions: vec![BitmapRegion {
                pixels: Arc::new(vec![tag; 2 * 2 * 4]),
                width: 2,
                height: 2,
                x: 0,
                y: 0,
                render_width: 2,
                render_height: 2,
            }],
        }]
    }
}

fn packet(tag: u8, rt_ms: u64) -> BitmapPacket {
    BitmapPacket {
        format: BitmapFormat::Pgs,
        data: gst::Buffer::from_slice(vec![tag, 0xAA]),
        codec_data: None,
        rt: gst::ClockTime::from_mseconds(rt_ms),
        duration: None,
    }
}

/// The decode worker retires when idle and the next packet still decodes.
///
/// A retirement races every submission. A packet handed to an inbox nobody
/// will read again is lost silently, so after the retirement the assertion is
/// the result (a decoded region on screen), not the thread count.
#[test]
fn an_idle_decode_worker_retires_and_comes_back() {
    gst::init().expect("gst init");
    let engine = CueEngine::new();
    engine.set_worker_idle_for_test(IDLE);
    let pushes = Arc::new(AtomicU64::new(0));
    let counter = pushes.clone();
    engine.set_decoder_factory(move |_format| {
        let decoder: Box<dyn SubpicDecoder> = Box::new(TagDecoder {
            pushes: counter.clone(),
        });
        Some(decoder)
    });

    engine.submit_bitmap(packet(1, 0));
    wait_for("the first packet to decode", || {
        !engine
            .overlays_for(Some(gst::ClockTime::from_mseconds(100)))
            .is_empty()
    });
    assert!(engine.workers_live().1, "the decode worker never started");
    #[cfg(target_os = "linux")]
    assert_eq!(threads_named("fvid-sub-decode"), 1);

    wait_for("the idle decode worker to retire", || {
        !engine.workers_live().1
    });
    #[cfg(target_os = "linux")]
    wait_for("the decode worker's thread to end", || {
        threads_named("fvid-sub-decode") == 0
    });

    // Respawn must be transparent.
    engine.submit_bitmap(packet(2, 2_000));
    wait_for("a packet submitted after the retirement to decode", || {
        engine
            .overlays_for(Some(gst::ClockTime::from_mseconds(2_100)))
            .first()
            .is_some_and(|overlay| overlay.pixels[0] == 2)
    });
    assert!(engine.workers_live().1);
    assert_eq!(
        pushes.load(Ordering::Relaxed),
        2,
        "a submission was lost across the retirement"
    );
}

/// The raster worker. Every video sink that ever showed a text cue has one,
/// so this retirement matters most on a device.
#[test]
fn an_idle_raster_worker_retires_and_comes_back() {
    gst::init().expect("gst init");
    let engine = CueEngine::new();
    engine.set_worker_idle_for_test(IDLE);
    // Without a canvas no raster is ever asked for and the worker under test
    // would never start.
    engine.set_canvas(1920, 1080);

    let cue = |text: &str, start: u64| CueInput {
        format: TextFormat::Utf8,
        text: text.to_owned(),
        start_rt: gst::ClockTime::from_mseconds(start),
        end_rt: Some(gst::ClockTime::from_mseconds(start + 4_000)),
    };

    // Warm first, as the video sink does at construction, so the respawn has
    // something to inherit.
    engine.warm();
    wait_for("the fontmap to warm", || engine.warm_up_time().is_some());
    let first_warm = engine.warm_up_time().expect("warmed");

    engine.submit(cue("before", 0));
    wait_for("the first cue to render", || {
        !engine
            .overlays_for(Some(gst::ClockTime::from_mseconds(100)))
            .is_empty()
    });
    assert!(engine.workers_live().0, "the raster worker never started");
    #[cfg(target_os = "linux")]
    assert_eq!(threads_named("fvid-cue-raster"), 1);

    wait_for("the idle raster worker to retire", || {
        !engine.workers_live().0
    });
    #[cfg(target_os = "linux")]
    wait_for("the raster worker's thread to end", || {
        threads_named("fvid-cue-raster") == 0
    });

    // A respawn inherits the warm-up. The retired worker's fontmap died with
    // its thread. The new one must rebuild it before being asked for pixels,
    // or the first cue after an idle gap pays the fontconfig cost `warm()`
    // exists to avoid. Observed as a fresh warm-up measurement.
    engine.submit(cue("after", 6_000));
    // The frame walk asks for the raster, so drive frames the way a sink does.
    // Nothing spawns a worker until something wants pixels.
    wait_for("a cue submitted after the retirement to render", || {
        !engine
            .overlays_for(Some(gst::ClockTime::from_mseconds(6_100)))
            .is_empty()
    });
    assert!(engine.workers_live().0);
    assert!(
        engine.warm_up_time().is_some_and(|warm| warm != first_warm),
        "the respawned worker never rebuilt its fontmap: the first cue after an idle gap pays \
         the cost `warm()` exists to keep off the cue path"
    );
}
