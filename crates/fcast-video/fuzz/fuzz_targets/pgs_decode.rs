//! PGS, raw bytes: arbitrary input chunked into packets and pushed at a fresh
//! PGS decoder.
//!
//! The unstructured half. It spends most of its time being rejected by the
//! framing, which is what makes it worth running. The framing is the only
//! code every byte of a hostile stream reaches, and the carry it maintains
//! across packets is the one piece of decoder state a single packet cannot
//! exercise. `pgs_decode_structured` is the other half, gets past the framing
//! on purpose, and is where the fuzzed geometry lives.
//!
//! The input is the raw byte string with NO wrapper, so a seed file is
//! literally a display set and a crashing artifact is literally the bytes
//! that crashed. Fragmentation comes from the chunking below, geometry from
//! the structured target's fuzzed canvas.

#![no_main]

use fcast_video::subpic::{
    BitmapFormat, SubpicDecoder,
    pgs::{ALLOCATION_BUDGET, PgsDecoder, fixtures},
};
use fcast_video_fuzz as harness;
use libfuzzer_sys::fuzz_target;

/// Bytes per packet.
///
/// Small enough that any display set worth the name is split across several.
/// A segment straddling a packet boundary is the carry's only path and no
/// single-packet input reaches it. Pairs of packets share a running time
/// (below), so a set split in two is reassembled and a set split in three is
/// not, putting both halves of the carry's contract on this path.
const PACKET: usize = 64;

fuzz_target!(|data: &[u8]| {
    harness::init();
    let video = harness::DEFAULT_VIDEO;
    let mut decoder = PgsDecoder::new();
    decoder.set_video_size(video.0, video.1);

    for (index, bytes) in data.chunks(PACKET).enumerate() {
        let packet = harness::packet(BitmapFormat::Pgs, bytes, index as u64 / 2);
        harness::push_and_check(
            &mut decoder,
            &packet,
            video,
            ALLOCATION_BUDGET,
            |decoder: &PgsDecoder| (decoder.held_bytes(), decoder.allocated_bytes()),
        );
    }

    harness::assert_recovers(
        &mut decoder,
        BitmapFormat::Pgs,
        &fixtures::minimal_display_set(),
        1,
        video,
        ALLOCATION_BUDGET,
    );
});
