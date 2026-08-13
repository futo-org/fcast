//! Decode-latency instrument, per format.
//!
//! Decoders run on the `fvid-sub-decode` worker, off the pipeline and render
//! threads, so a slow decode costs a late subtitle rather than a dropped
//! frame. The ceiling is generous on purpose. It catches order-of-magnitude
//! regressions (accidental quadratics, lost caching), not drift.
//!
//! Numbers are printed as well as asserted, for manual inspection with
//! `--nocapture`.
//!
//! ```sh
//! cargo test -p fcast-video --test decode_latency -- --nocapture
//! ```

use std::time::{Duration, Instant};

use fcast_video::subpic::{BitmapFormat, BitmapPacket, decoder_for, dvb, pgs, vobsub};

/// A warm decode should take single-digit milliseconds. 100ms is where a
/// viewer would notice.
const CEILING: Duration = Duration::from_millis(100);

/// Decodes per format. Enough for a meaningful p99, few enough to stay fast.
const RUNS: usize = 200;

fn packet(format: BitmapFormat, bytes: &[u8], rt_ms: u64) -> BitmapPacket {
    BitmapPacket {
        format,
        data: gst::Buffer::from_slice(bytes.to_vec()),
        codec_data: None,
        rt: gst::ClockTime::from_mseconds(rt_ms),
        duration: None,
    }
}

#[test]
fn a_warm_decode_costs_well_under_a_frame_in_every_format() {
    gst::init().expect("gst init");

    let cases: [(BitmapFormat, Vec<u8>, Option<&[u8]>); 3] = [
        (
            BitmapFormat::Pgs,
            pgs::fixtures::minimal_display_set(),
            None,
        ),
        (
            BitmapFormat::Vobsub,
            vobsub::fixtures::minimal_unit(),
            Some(vobsub::fixtures::SAMPLE_IDX),
        ),
        (
            BitmapFormat::Dvb,
            // The grounded set exercises the whole decode, not a fragment.
            dvb::fixtures::grounded_display_set(),
            None,
        ),
    ];

    for (format, bytes, codec_data) in cases {
        let mut decoder =
            decoder_for(format).unwrap_or_else(|| panic!("{format:?} has no decoder"));
        decoder.set_video_size(1920, 1080);
        if let Some(codec_data) = codec_data {
            decoder.set_codec_data(codec_data);
        }

        // Warm first. The first DVB set builds state later sets paint into,
        // so timing it would time a different operation.
        let mut rt = 0u64;
        for _ in 0..8 {
            rt += 1_000;
            decoder.push(&packet(format, &bytes, rt));
        }

        let mut costs = Vec::with_capacity(RUNS);
        let mut drew = 0usize;
        for _ in 0..RUNS {
            rt += 1_000;
            let at = Instant::now();
            let updates = decoder.push(&packet(format, &bytes, rt));
            costs.push(at.elapsed());
            drew += updates.iter().filter(|u| !u.regions.is_empty()).count();
        }
        assert!(
            drew >= RUNS,
            "{format:?}: only {drew} of {RUNS} decodes drew anything, so this timed a refusal"
        );

        costs.sort_unstable();
        let p50 = costs[costs.len() / 2];
        let p99 = costs[(costs.len() * 99).div_ceil(100).saturating_sub(1)];
        println!(
            "{format:?}: p50 {p50:?} p99 {p99:?} max {:?} over {RUNS} warm decodes",
            costs[costs.len() - 1]
        );
        assert!(
            p99 < CEILING,
            "{format:?}: p99 {p99:?} is past the {CEILING:?} ceiling, this is an \
             order-of-magnitude change, not drift"
        );
    }
}
