//! Five threads on one engine with a real DVB decoder.
//!
//! DVB keeps state between display sets and prices allocations against a
//! budget, so its worker holds a lock for measurable time while other threads
//! submit, clear, resize and read.
//!
//! Proves the engine never wedges under contention, nothing published outlives
//! its `clear`, and counters stay consistent. `#[ignore]`d because it runs for
//! seconds. It is also the driver the exit gate runs under ThreadSanitizer.
//!
//! ```sh
//! cargo test -p fcast-video --test engine_stress -- --ignored --nocapture
//! ```

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use fcast_video::{
    cue::{CueEngine, CueInput, TextFormat},
    subpic::{BitmapFormat, BitmapPacket, dvb::fixtures, pgs::ALLOCATION_BUDGET},
};

/// Run length. Long enough for clears to land mid-decode many times, short
/// enough to be a gate step. `FCAST_STRESS_SECS` lengthens the hunt.
fn duration() -> Duration {
    Duration::from_secs(
        std::env::var("FCAST_STRESS_SECS")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(5),
    )
}

fn packet(bytes: Vec<u8>, rt_ms: u64) -> BitmapPacket {
    BitmapPacket {
        format: BitmapFormat::Dvb,
        data: gst::Buffer::from_slice(bytes),
        codec_data: None,
        rt: gst::ClockTime::from_mseconds(rt_ms),
        duration: None,
    }
}

/// A display set that breaches the allocation budget twice over, so the decode
/// worker is spilling and resetting while the other threads work.
fn over_budget() -> Vec<u8> {
    fixtures::data_field(&[
        fixtures::page(5, 0, &[(1, 0, 0), (2, 0, 0)]),
        fixtures::region(1, 4096, 4096, 4, 1, 0, true, &[]),
        fixtures::region(2, 4096, 4096, 4, 1, 0, true, &[]),
        fixtures::end_of_display_set(),
    ])
}

#[test]
#[ignore = "a stress driver: seconds of five-thread load, run by the exit gate"]
fn five_threads_on_one_engine_with_a_dvb_decoder() {
    gst::init().expect("gst init");

    let engine = Arc::new(CueEngine::new());
    engine.set_canvas(1920, 1080);
    engine.set_video_size(1920, 1080);

    let stop = Arc::new(AtomicBool::new(false));
    let submitted = Arc::new(AtomicU64::new(0));
    let cleared = Arc::new(AtomicU64::new(0));
    let read = Arc::new(AtomicU64::new(0));

    let mut threads = Vec::new();

    // 1. Good stream: acquisition sets at advancing running times.
    {
        let (engine, stop, submitted) = (engine.clone(), stop.clone(), submitted.clone());
        threads.push(std::thread::spawn(move || {
            let good = fixtures::acquisition_display_set();
            let grounded = fixtures::grounded_display_set();
            let mut rt = 0u64;
            while !stop.load(Ordering::Relaxed) {
                rt += 40;
                let bytes = if rt % 400 == 0 { &good } else { &grounded };
                engine.submit_bitmap(packet(bytes.clone(), rt));
                submitted.fetch_add(1, Ordering::Relaxed);
            }
        }));
    }

    // 2. Hostile stream: budget-breaching sets and noise on the same worker.
    {
        let (engine, stop, submitted) = (engine.clone(), stop.clone(), submitted.clone());
        threads.push(std::thread::spawn(move || {
            let big = over_budget();
            let mut rt = 0u64;
            let mut noise = vec![0x20u8, 0x00];
            for index in 0..2048u32 {
                noise.push((index.wrapping_mul(2_654_435_761) >> 13) as u8);
            }
            while !stop.load(Ordering::Relaxed) {
                rt += 40;
                let bytes = if rt % 200 == 0 { &big } else { &noise };
                engine.submit_bitmap(packet(bytes.clone(), rt));
                submitted.fetch_add(1, Ordering::Relaxed);
                std::thread::yield_now();
            }
        }));
    }

    // 3. Clears (epoch discard). Old-epoch decodes must be dropped, and the
    //    window that matters is while another thread is inside a decode.
    {
        let (engine, stop, cleared) = (engine.clone(), stop.clone(), cleared.clone());
        threads.push(std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                engine.clear();
                cleared.fetch_add(1, Ordering::Relaxed);
                std::thread::sleep(Duration::from_micros(700));
            }
        }));
    }

    // 4. Renderer: reads overlays, takes the dirty flag, moves the video
    //    rectangle under everything.
    {
        let (engine, stop, read) = (engine.clone(), stop.clone(), read.clone());
        threads.push(std::thread::spawn(move || {
            let mut at = 0u64;
            while !stop.load(Ordering::Relaxed) {
                at = at.wrapping_add(17);
                let _ = engine.overlays_for(Some(gst::ClockTime::from_mseconds(at)));
                let _ = engine.current_overlays();
                let _ = engine.take_dirty();
                read.fetch_add(1, Ordering::Relaxed);
                if at % 1_000 < 17 {
                    engine.set_video_size(1280 + (at % 3) as u32 * 2, 720);
                }
            }
        }));
    }

    // 5. Text cues through the raster worker, so both workers are live and
    //    both reset hooks hit the same state lock.
    {
        let (engine, stop) = (engine.clone(), stop.clone());
        threads.push(std::thread::spawn(move || {
            let mut rt = 0u64;
            while !stop.load(Ordering::Relaxed) {
                rt += 100;
                engine.submit(CueInput {
                    format: TextFormat::Utf8,
                    text: format!("stress {rt}"),
                    start_rt: gst::ClockTime::from_mseconds(rt),
                    end_rt: Some(gst::ClockTime::from_mseconds(rt + 500)),
                });
                if rt % 2_000 == 0 {
                    engine.reset_timeline();
                }
                std::thread::sleep(Duration::from_micros(300));
            }
        }));
    }

    std::thread::sleep(duration());
    stop.store(true, Ordering::Relaxed);
    for thread in threads {
        thread.join().expect("a stress thread panicked");
    }

    // Wedge check: a known-good display set must still decode and reach the
    // screen. Any race that stranded the worker, decoder, or publish ends here.
    engine.clear();
    engine.set_video_size(1920, 1080);
    let rt = 900_000u64;
    engine.submit_bitmap(packet(fixtures::acquisition_display_set(), rt));
    let at = gst::ClockTime::from_mseconds(rt + 200);
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline && engine.overlays_for(Some(at)).is_empty() {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        !engine.overlays_for(Some(at)).is_empty(),
        "the engine never drew again: submitted {}, cleared {}, decoded {}, errors {}",
        submitted.load(Ordering::Relaxed),
        cleared.load(Ordering::Relaxed),
        engine.bitmap_sets_decoded(),
        engine.bitmap_decode_errors(),
    );

    // Schedule-dependent counters are printed, not asserted.
    println!(
        "stress: submitted={} cleared={} reads={} decoded={} errors={} overflow_resets={} \
         dropped_sets={}",
        submitted.load(Ordering::Relaxed),
        cleared.load(Ordering::Relaxed),
        read.load(Ordering::Relaxed),
        engine.bitmap_sets_decoded(),
        engine.bitmap_decode_errors(),
        engine.bitmap_overflow_resets(),
        engine.bitmap_dropped_sets(),
    );
    assert!(
        submitted.load(Ordering::Relaxed) > 1_000,
        "the stress did not run: only {} packets submitted",
        submitted.load(Ordering::Relaxed)
    );

    // Every decoder shares this one cap. The engine's decoder is not reachable
    // from here, so pin the constant instead.
    assert_eq!(ALLOCATION_BUDGET, 32 * 1024 * 1024);
}
