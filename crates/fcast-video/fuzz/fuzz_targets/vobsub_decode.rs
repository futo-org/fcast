//! VOBSUB: a fuzzed `.idx` and a fuzzed subpicture unit, in that order.
//!
//! VOBSUB is the one format whose palette does not travel in the stream, so
//! the text the container hands over is attacker-controlled input on a path
//! the other decoders do not have. Splitting the fuzzer's bytes into an idx
//! half and a packet half puts both under it at once: a palette parsed out of
//! nonsense, applied to a picture built out of different nonsense.
//!
//! The standing invariants come from the shared harness. This target adds
//! `set_codec_data` called with arbitrary bytes, which is what a container
//! that lies would do.

#![no_main]

use fcast_video::subpic::{
    BitmapFormat, SubpicDecoder,
    pgs::ALLOCATION_BUDGET,
    vobsub::{VobsubDecoder, fixtures},
};
use fcast_video_fuzz as harness;
use libfuzzer_sys::fuzz_target;

#[derive(Debug, arbitrary::Arbitrary)]
struct Input<'a> {
    video_width: u16,
    video_height: u16,
    /// The container's `.idx` text, or bytes pretending to be one.
    idx: &'a [u8],
    /// Whether to teach the decoder that idx at all. A stream with no palette
    /// takes a different path (the guessed grey ramp) which must be as safe
    /// as the other.
    teach_idx: bool,
    packets: Vec<&'a [u8]>,
}

fuzz_target!(|input: Input<'_>| {
    harness::init();
    let video = harness::video_size(input.video_width, input.video_height);
    let mut decoder = VobsubDecoder::new();
    decoder.set_video_size(video.0, video.1);
    if input.teach_idx {
        decoder.set_codec_data(input.idx);
    }

    for (index, bytes) in input.packets.iter().enumerate() {
        // A subpicture unit is self-contained, so every packet is its own
        // display. The running times only have to be distinct.
        let packet = harness::packet(BitmapFormat::Vobsub, bytes, index as u64 * 100);
        harness::push_and_check(
            &mut decoder,
            &packet,
            video,
            ALLOCATION_BUDGET,
            |decoder: &VobsubDecoder| (decoder.held_bytes(), decoder.allocated_bytes()),
        );
    }

    // THE RECOVERY INVARIANT. The known-good unit needs the known-good
    // palette with it. Picture and colours arrive by different roads, and a
    // decoder left holding a fuzzed palette would draw the right picture in
    // the wrong colours rather than fail.
    decoder.set_codec_data(fixtures::SAMPLE_IDX);
    harness::assert_recovers(
        &mut decoder,
        BitmapFormat::Vobsub,
        &fixtures::minimal_unit(),
        1,
        video,
        ALLOCATION_BUDGET,
    );
});
