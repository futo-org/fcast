//! Drives an out-of-crate [`SubpicDecoder`] through the public seam to an
//! overlay, linking `fcast-video` the way any dependent does.
//!
//! Exists to keep a visibility decision honest. The seams
//! [`CueEngine::set_decoder_factory`] and [`CueEngine::hold_decode_for_test`]
//! must stay `#[doc(hidden)] pub`, not `cfg(test)`, because `cfg(test)` items
//! are invisible from `tests/`. Narrowing either breaks this file's build,
//! which is the point.

use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use fcast_video::{
    cue::CueEngine,
    subpic::{BitmapFormat, BitmapPacket, BitmapRegion, DisplayUpdate, SubpicDecoder},
    video::OverlaySpace,
};

/// A formatless decoder. Each packet's first byte becomes one 4x4 region of
/// that value, enough to see the engine schedule and composite it.
struct TagDecoder {
    pushes: Arc<AtomicU64>,
    size: Option<(u32, u32)>,
}

impl SubpicDecoder for TagDecoder {
    fn set_codec_data(&mut self, _data: &[u8]) {}

    fn set_video_size(&mut self, width: u32, height: u32) {
        self.size = Some((width, height));
    }

    fn push(&mut self, packet: &BitmapPacket) -> Vec<DisplayUpdate> {
        self.pushes.fetch_add(1, Ordering::Relaxed);
        let tag = packet
            .data
            .map_readable()
            .ok()
            .and_then(|map| map.first().copied())
            .unwrap_or(0);
        // The coded size the engine taught must reach the placement.
        let (width, height) = self.size.unwrap_or((1, 1));
        vec![DisplayUpdate {
            start_rt: packet.rt,
            end_rt: None,
            regions: vec![BitmapRegion {
                pixels: Arc::new(vec![tag; 4 * 4 * 4]),
                width: 4,
                height: 4,
                x: (width / 2) as i32,
                y: (height / 2) as i32,
                render_width: 4,
                render_height: 4,
            }],
        }]
    }

    fn reset(&mut self) {
        self.size = None;
    }
}

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

fn packet(tag: u8, rt: u64) -> BitmapPacket {
    BitmapPacket {
        format: BitmapFormat::Pgs,
        data: gst::Buffer::from_slice(vec![tag, 0xAA]),
        codec_data: None,
        rt: gst::ClockTime::from_mseconds(rt),
        duration: None,
    }
}

#[test]
fn an_out_of_crate_decoder_reaches_the_screen_through_the_public_seam() {
    gst::init().expect("gst init");

    let engine = CueEngine::new();
    engine.set_video_size(1920, 1080);
    let pushes = Arc::new(AtomicU64::new(0));
    let counter = pushes.clone();
    engine.set_decoder_factory(move |format| {
        assert_eq!(format, BitmapFormat::Pgs);
        let decoder: Box<dyn SubpicDecoder> = Box::new(TagDecoder {
            pushes: counter.clone(),
            size: None,
        });
        Some(decoder)
    });

    // The latch lets a test fill the inbox to a known depth by stopping the
    // worker from draining it.
    let hold = engine.hold_decode_for_test();
    engine.submit_bitmap(packet(9, 0));
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(
        pushes.load(Ordering::Relaxed),
        0,
        "the decode worker ran while it was held"
    );
    drop(hold);

    wait_for("the decoded region to reach the overlay set", || {
        !engine
            .overlays_for(Some(gst::ClockTime::from_mseconds(10)))
            .is_empty()
    });

    let overlays = engine.overlays_for(Some(gst::ClockTime::from_mseconds(10)));
    assert_eq!(overlays.len(), 1);
    assert_eq!(
        overlays[0].space,
        OverlaySpace::SrcFrame,
        "a bitmap region is composited in the picture's own space"
    );
    assert_eq!(overlays[0].pixels[0], 9, "these are the decoder's pixels");
    assert_eq!(
        (overlays[0].x, overlays[0].y),
        (960, 540),
        "the coded size the engine was told never reached the decoder"
    );
}
