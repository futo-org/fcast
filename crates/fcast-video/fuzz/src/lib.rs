//! The shared half of the bitmap-subtitle fuzz harness.
//!
//! Every target is one file in `fuzz_targets/` and everything a target does
//! beyond building its own bytes is here, so a new format is a new target
//! file and byte builder, not a new set of invariants.
//!
//! # What the targets assert
//!
//! A fuzz target that only asserts "did not panic" finds crashes and nothing
//! else. Three properties are asserted for EVERY input:
//!
//!  1. **The allocation cap holds.** Stream bytes are untrusted and every
//!     format has a field that says "allocate this much". [`held_bytes`] is
//!     checked after each packet against the decoder's published budget.
//!  2. **Emitted regions are internally consistent.** `BitmapRegion` pixels
//!     go straight to a GPU upload, so a buffer that does not match its
//!     dimensions is a read past the end of an allocation in the renderer.
//!     [`check_update`].
//!  3. **A reset leaves the decoder usable.** The error policy is that a
//!     malformed set is a counted reset and the next set recovers. A decoder
//!     that quietly stops decoding after garbage never panics and is useless.
//!     [`assert_recovers`] feeds a known-good display set at the end of every
//!     run.

use fcast_video::subpic::{BitmapFormat, BitmapPacket, DisplayUpdate, SubpicDecoder};

/// The video size every target teaches its decoder unless it fuzzes one.
pub const DEFAULT_VIDEO: (u32, u32) = (1920, 1080);

/// Initialise GStreamer once per process.
///
/// The decoders take a `gst::Buffer`, and a miniobject cannot be built before
/// the type system it belongs to is registered. Without this the first
/// `Buffer::from_slice` takes the process down with a signal.
pub fn init() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| gst::init().expect("gst init"));
}

/// A running time far past anything a run generates, so the known-good tail
/// never shares a packet's time (and so is never appended to a half-read
/// segment carried over from one).
pub const RECOVERY_RT_MS: u64 = 1_000_000;

/// Bring a fuzzed video size into the range the engine actually passes. It
/// drops zero dimensions at its own door and a coded size is never absurd.
pub fn video_size(width: u16, height: u16) -> (u32, u32) {
    (1 + u32::from(width) % 4096, 1 + u32::from(height) % 4096)
}

pub fn packet(format: BitmapFormat, bytes: &[u8], rt_ms: u64) -> BitmapPacket {
    BitmapPacket {
        format,
        data: gst::Buffer::from_slice(bytes.to_vec()),
        codec_data: None,
        rt: gst::ClockTime::from_mseconds(rt_ms),
        duration: None,
    }
}

/// Everything that must be true of one decoded display set.
///
/// `video` is the size the decoder was taught. Rects are in video pixels by
/// contract, so a region that lands outside the picture is either a geometry
/// bug or a region that should never have been emitted.
pub fn check_update(update: &DisplayUpdate, video: (u32, u32), budget: u64) {
    if let Some(end) = update.end_rt {
        assert!(
            end >= update.start_rt,
            "a display set that ends before it starts: {:?}..{end}",
            update.start_rt
        );
    }
    for region in &update.regions {
        assert!(
            region.width > 0 && region.height > 0,
            "a region with no pixels in it: {}x{}",
            region.width,
            region.height
        );
        let expected = region.width as usize * region.height as usize * 4;
        assert_eq!(
            region.pixels.len(),
            expected,
            "a region's buffer does not match its dimensions ({}x{}): the renderer would read \
             past the end of it",
            region.width,
            region.height
        );
        assert!(
            region.pixels.len() as u64 <= budget,
            "a region larger than the whole allocation budget"
        );
        assert!(
            region.render_width > 0 && region.render_height > 0,
            "a region asked to render into nothing"
        );
        assert!(
            region.x >= 0 && region.y >= 0,
            "a region placed off the top-left of the picture: {},{}",
            region.x,
            region.y
        );
        // The rect must be CONTAINED in the picture, not merely start inside
        // it. A well-formed canvas smaller than its own objects would
        // otherwise produce a rect wider than the picture. The decoders clip,
        // and this asserts it for every input.
        assert!(
            (region.x as u32) < video.0 && (region.y as u32) < video.1,
            "a region placed off the picture ({},{} against {}x{}): it can never be seen",
            region.x,
            region.y,
            video.0,
            video.1
        );
        assert!(
            region.x as u32 + region.render_width <= video.0
                && region.y as u32 + region.render_height <= video.1,
            "a region's rect runs off the picture ({},{} {}x{} against {}x{})",
            region.x,
            region.y,
            region.render_width,
            region.render_height,
            video.0,
            video.1
        );
    }
}

/// Drive one packet and check everything it produced. `held` reports what the
/// decoder is holding after the push, read off the concrete decoder type
/// because accounting is per-format and the trait deliberately does not carry
/// it.
pub fn push_and_check<D: SubpicDecoder>(
    decoder: &mut D,
    packet: &BitmapPacket,
    video: (u32, u32),
    budget: u64,
    accounting: impl Fn(&D) -> (u64, u64),
) -> Vec<DisplayUpdate> {
    let updates = decoder.push(packet);
    for update in &updates {
        check_update(update, video, budget);
    }
    // `(what the decoder says it holds, what it has actually taken from the
    // allocator)`. Both are checked because the charged figure can undercount
    // real allocation (spare capacity, `Vec` growth), and a cap is only worth
    // what its accounting is.
    let (held, allocated) = accounting(decoder);
    assert!(
        held <= budget,
        "the decoder is holding {held} bytes against a budget of {budget}"
    );
    assert!(
        allocated <= held,
        "the decoder has taken {allocated} bytes from the allocator and charged itself {held}: \
         the cap is being enforced on a number that is not the memory"
    );
    updates
}

/// THE RECOVERY INVARIANT: after whatever the run just did, a known-good
/// display set still decodes into the regions it describes.
///
/// This is the half of the error policy that "no panic" does not cover. Every
/// malformed structure is contracted to be a counted reset with the next set
/// recovering. A decoder that wedges (a stuck carry, a poisoned object store,
/// an open set that never closes) passes a no-panic fuzzer forever while
/// showing no subtitles at all.
pub fn assert_recovers<D: SubpicDecoder>(
    decoder: &mut D,
    format: BitmapFormat,
    good: &[u8],
    regions: usize,
    video: (u32, u32),
    budget: u64,
) {
    recovers(decoder, format, good, regions, video, budget, Some(1));
}

/// The same, for a format that may legitimately answer with MORE than the good
/// set's own update.
///
/// DVB force-terminates whatever display set was open when a packet arrives
/// at a new time, so it answers with the abandoned set AND the good one. The
/// count stays exact for every other format so that a helper tolerating extra
/// updates does not stop noticing when a format grows one.
pub fn assert_recovers_last<D: SubpicDecoder>(
    decoder: &mut D,
    format: BitmapFormat,
    good: &[u8],
    regions: usize,
    video: (u32, u32),
    budget: u64,
) {
    recovers(decoder, format, good, regions, video, budget, None);
}

fn recovers<D: SubpicDecoder>(
    decoder: &mut D,
    format: BitmapFormat,
    good: &[u8],
    regions: usize,
    video: (u32, u32),
    budget: u64,
    exactly: Option<usize>,
) {
    let updates = decoder.push(&packet(format, good, RECOVERY_RT_MS));
    for update in &updates {
        check_update(update, video, budget);
    }
    if let Some(exactly) = exactly {
        assert_eq!(
            updates.len(),
            exactly,
            "a known-good input answered with {} updates instead of {exactly}: either the \
             decoder is wedged or it has grown a behaviour nobody declared",
            updates.len()
        );
    }
    let Some(last) = updates.last() else {
        panic!("a known-good input did not decode after the fuzzed one: the decoder is wedged");
    };
    assert_eq!(
        last.regions.len(),
        regions,
        "the known-good input decoded into the wrong number of regions"
    );
}

/// Push an input that is NOT expected to decode into anything in particular,
/// and check everything that must hold anyway.
///
/// The state-poisoning probe. A recovery tail that begins by resetting the
/// decoder proves the parser survived but nothing about the state the fuzzer
/// left behind, because that state is thrown away before the good input is
/// read. So a non-resetting input goes first.
///
/// It also has to draw. The input states its own display grid, so nothing the
/// fuzzer did to the geometry can excuse a blank. The number of regions is
/// checked on the LAST update for the same reason [`assert_recovers_last`]
/// exists: a packet at a new running time may force-terminate a set the
/// fuzzer left open, and that abandoned set comes out first.
pub fn assert_survives<D: SubpicDecoder>(
    decoder: &mut D,
    format: BitmapFormat,
    input: &[u8],
    regions: usize,
    video: (u32, u32),
    budget: u64,
    accounting: impl Fn(&D) -> (u64, u64),
) {
    let updates = push_and_check(
        decoder,
        &packet(format, input, RECOVERY_RT_MS - 1),
        video,
        budget,
        accounting,
    );
    let Some(last) = updates.last() else {
        panic!(
            "a display set that names its own grid decoded into nothing: the state the fuzzed              input left behind has wedged the decoder"
        );
    };
    assert_eq!(
        last.regions.len(),
        regions,
        "the non-resetting display set decoded into the wrong number of regions"
    );
}
