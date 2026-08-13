//! DVB: fuzzed SEGMENTS in valid framing, against the decoder that keeps the
//! most state.
//!
//! The marquee invariant here is the ALLOCATION one. DVB is the only format
//! whose buffers persist across display sets and grow on the stream's
//! say-so. A region composition names a size and the decoder holds that many
//! bytes until something says otherwise, a page can carry many regions, and
//! an object segment paints into whichever of them claimed it.
//! Under-accounting has the most room to hide here, so
//! `allocated_bytes() <= held_bytes()` is checked after every packet rather
//! than at the end.
//!
//! Structure-aware for the same reason the PGS target is. The framing is
//! `[0x0f][type][page id][length]` after two identifying bytes, so random
//! input dies at the first sync byte and never reaches a segment parser. The
//! fuzzer chooses what goes inside the frame, including, through
//! `Shaped::Raw`, a segment type and payload with no structure at all.

#![no_main]

use fcast_video::subpic::{
    BitmapFormat, SubpicDecoder,
    dvb::{DvbDecoder, fixtures},
    pgs::ALLOCATION_BUDGET,
};
use fcast_video_fuzz as harness;
use libfuzzer_sys::fuzz_target;

#[derive(Debug, arbitrary::Arbitrary)]
enum Shaped {
    Page {
        timeout: u8,
        state: u8,
        regions: Vec<(u8, u16, u16)>,
    },
    Region {
        id: u8,
        width: u16,
        height: u16,
        depth: u8,
        clut: u8,
        background: u8,
        fill: bool,
        objects: Vec<(u16, u16, u16)>,
    },
    Clut {
        id: u8,
        entries: Vec<(u8, u8, u8, u8, u8)>,
    },
    /// An object whose two fields are fuzzed bytes: the pixel-data blocks and
    /// the three run-length codings are behind this one.
    Object {
        id: u16,
        top: Vec<u8>,
        bottom: Vec<u8>,
    },
    /// The same, but with fields built from real pixel strings, so the run
    /// decoders are reached without the fuzzer having to discover their bit
    /// patterns.
    ObjectRuns {
        id: u16,
        depth_hint: u8,
        runs: Vec<(u8, u16)>,
    },
    DisplayDefinition {
        width: u16,
        height: u16,
        window: Option<(u16, u16, u16, u16)>,
    },
    End,
    Raw {
        kind: u8,
        payload: Vec<u8>,
    },
}

impl Shaped {
    fn bytes(&self) -> Vec<u8> {
        match self {
            Self::Page {
                timeout,
                state,
                regions,
            } => fixtures::page(*timeout, *state, &regions[..regions.len().min(64)]),
            Self::Region {
                id,
                width,
                height,
                depth,
                clut,
                background,
                fill,
                objects,
            } => fixtures::region(
                *id,
                *width,
                *height,
                match depth % 3 {
                    0 => 2,
                    1 => 4,
                    _ => 8,
                },
                *clut,
                *background,
                *fill,
                // Up to what one segment can carry. The budget's worst case is
                // ten thousand placements in one segment, and a target that
                // cannot build the input cannot find the defect.
                &objects[..objects.len().min(10_920)],
            ),
            Self::Clut { id, entries } => fixtures::clut(*id, &entries[..entries.len().min(256)]),
            Self::Object { id, top, bottom } => fixtures::object(
                *id,
                &top[..top.len().min(0x4000)],
                &bottom[..bottom.len().min(0x4000)],
            ),
            Self::ObjectRuns {
                id,
                depth_hint,
                runs,
            } => {
                let runs: Vec<(u8, u32)> = runs
                    .iter()
                    .take(64)
                    .map(|(index, length)| (*index, u32::from(*length) % 300))
                    .collect();
                let field = match depth_hint % 3 {
                    0 => fixtures::two_bit_string(&runs),
                    1 => fixtures::four_bit_string(&runs),
                    _ => fixtures::eight_bit_string(&runs),
                };
                fixtures::object(*id, &field, &field)
            }
            Self::DisplayDefinition {
                width,
                height,
                window,
            } => fixtures::display_definition((*width).max(1), (*height).max(1), *window),
            Self::End => fixtures::end_of_display_set(),
            Self::Raw { kind, payload } => {
                fixtures::segment(*kind, &payload[..payload.len().min(0xFFFF)])
            }
        }
    }
}

#[derive(Debug, arbitrary::Arbitrary)]
struct Input {
    video_width: u16,
    video_height: u16,
    /// Each inner run is one packet's worth of segments. Packets are paired
    /// onto running times, so a display set spanning two of them is
    /// reachable, and so is the force-terminate a third one triggers.
    packets: Vec<Vec<Shaped>>,
}

fuzz_target!(|input: Input| {
    harness::init();
    let video = harness::video_size(input.video_width, input.video_height);
    let mut decoder = DvbDecoder::new();
    decoder.set_video_size(video.0, video.1);

    for (index, segments) in input.packets.iter().enumerate() {
        let mut bytes = vec![0x20u8, 0x00];
        for segment in segments.iter().take(64) {
            bytes.extend_from_slice(&segment.bytes());
        }
        let packet = harness::packet(BitmapFormat::Dvb, &bytes, index as u64 / 2);
        harness::push_and_check(
            &mut decoder,
            &packet,
            video,
            ALLOCATION_BUDGET,
            |decoder: &DvbDecoder| (decoder.held_bytes(), decoder.allocated_bytes()),
        );
    }

    // THE RECOVERY INVARIANT. For this format it is a claim about the
    // persistent state as much as the parser. A display set has to decode
    // after whatever the fuzzer did to the regions, palettes and page
    // composition it left behind.
    //
    // FIRST, a display set that does NOT reset anything, so the state the
    // fuzzer left is still in place while it decodes. That is the only way a
    // state-poisoning defect is observable. It states its own grid and is
    // held to producing a picture, because a tail that asks only "did not
    // panic" is passed by a decoder wedged into drawing nothing.
    harness::assert_survives(
        &mut decoder,
        BitmapFormat::Dvb,
        &fixtures::grounded_display_set(),
        1,
        video,
        ALLOCATION_BUDGET,
        |decoder: &DvbDecoder| (decoder.held_bytes(), decoder.allocated_bytes()),
    );

    // THEN the acquisition set, which carries its own display grid and a
    // mode-change page, what a broadcast sends for a receiver that has just
    // tuned in. A decoder that cannot draw after one is wedged. The count is
    // relaxed here and only here because this format force-terminates
    // whatever was open when a packet arrives at a new time, so the abandoned
    // set comes out beside the good one.
    harness::assert_recovers_last(
        &mut decoder,
        BitmapFormat::Dvb,
        &fixtures::acquisition_display_set(),
        1,
        video,
        ALLOCATION_BUDGET,
    );
});
