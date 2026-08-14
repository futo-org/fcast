//! PGS, structure-aware: fuzzed SEGMENTS in valid framing, so the input reaches
//! the per-segment parsers instead of dying at a length check.
//!
//! The framing is `[type][length][payload]` with no sync word, which makes it
//! very cheap for random bytes to be rejected and very expensive for them to
//! reach the deeper parsers. This target builds the frame itself and lets the
//! fuzzer choose what goes inside it, including, through `Shaped::Raw`, a
//! segment type and payload with no structure at all.
//!
//! Two shapes get extra help because they are where the interesting state
//! lives. A presentation segment is built from fuzzed FIELDS, so canvases,
//! composition positions and crops are reached rather than guessed. An object
//! segment is built with a real 24-bit length header, without which
//! fragmentation is unreachable since a random length almost never matches
//! the bytes behind it.

#![no_main]

use fcast_video::subpic::{
    BitmapFormat, SubpicDecoder,
    pgs::{ALLOCATION_BUDGET, PgsDecoder, fixtures},
};
use fcast_video_fuzz as harness;
use libfuzzer_sys::fuzz_target;

#[derive(Debug, arbitrary::Arbitrary)]
struct Composition {
    id: u16,
    x: u16,
    y: u16,
    forced: bool,
    crop: Option<(u16, u16, u16, u16)>,
}

#[derive(Debug, arbitrary::Arbitrary)]
enum Shaped {
    Presentation {
        canvas_width: u16,
        canvas_height: u16,
        objects: Vec<Composition>,
    },
    Palette {
        entries: Vec<(u8, u8, u8, u8, u8)>,
    },
    Window {
        id: u8,
        x: u16,
        y: u16,
        width: u16,
        height: u16,
    },
    ObjectFirst {
        id: u16,
        version: u8,
        declared: u32,
        body: Vec<u8>,
    },
    ObjectMore {
        id: u16,
        version: u8,
        body: Vec<u8>,
    },
    /// An object whose declared length is the truth, so the object completes
    /// and the RLE expander runs.
    ObjectWhole {
        id: u16,
        version: u8,
        width: u16,
        height: u16,
        rle: Vec<u8>,
    },
    End,
    /// A segment type and payload with nothing shaped about them.
    Raw {
        kind: u8,
        payload: Vec<u8>,
    },
}

impl Shaped {
    fn bytes(&self) -> Vec<u8> {
        match self {
            Self::Presentation {
                canvas_width,
                canvas_height,
                objects,
            } => {
                let objects: Vec<_> = objects
                    .iter()
                    .take(255)
                    .map(|object| fixtures::Composed {
                        id: object.id,
                        x: object.x,
                        y: object.y,
                        forced: object.forced,
                        crop: object.crop,
                    })
                    .collect();
                fixtures::presentation((*canvas_width, *canvas_height), &objects)
            }
            Self::Palette { entries } => fixtures::palette(&entries[..entries.len().min(256)]),
            Self::Window {
                id,
                x,
                y,
                width,
                height,
            } => fixtures::window(*id, *x, *y, *width, *height),
            Self::ObjectFirst {
                id,
                version,
                declared,
                body,
            } => fixtures::object_first(
                *id,
                *version,
                (declared & 0xFF_FFFF) as usize,
                &body[..body.len().min(0xFF00)],
            ),
            Self::ObjectMore { id, version, body } => {
                fixtures::object_more(*id, *version, &body[..body.len().min(0xFF00)])
            }
            Self::ObjectWhole {
                id,
                version,
                width,
                height,
                rle,
            } => {
                let mut data = width.to_be_bytes().to_vec();
                data.extend_from_slice(&height.to_be_bytes());
                data.extend_from_slice(&rle[..rle.len().min(0xFF00)]);
                fixtures::object_first(*id, *version, data.len(), &data)
            }
            Self::End => fixtures::end(),
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
    /// Each inner run is one packet's worth of segments, so a display set
    /// spanning packets is reachable.
    packets: Vec<Vec<Shaped>>,
}

fuzz_target!(|input: Input| {
    harness::init();
    let video = harness::video_size(input.video_width, input.video_height);
    let mut decoder = PgsDecoder::new();
    decoder.set_video_size(video.0, video.1);

    for (index, segments) in input.packets.iter().enumerate() {
        let mut bytes = Vec::new();
        for segment in segments {
            bytes.extend_from_slice(&segment.bytes());
        }
        let packet = harness::packet(BitmapFormat::Pgs, &bytes, index as u64 / 2);
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
