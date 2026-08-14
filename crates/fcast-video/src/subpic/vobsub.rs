//! DVD subpicture subtitles (`subpicture/x-dvd`), a.k.a. VOBSUB.
//!
//! The oldest of the three formats and the only SELF-CONTAINED one: a single
//! packet (one "subpicture unit", one matroska block) carries the picture,
//! its palette indices, its position and its whole display schedule. Nothing
//! carries over from the packet before it. That is not a detail, it is the
//! reason this format can do something the other two cannot: a subtitle
//! redelivered by a seek regenerates completely from the one packet that
//! covers the position, so a paused track switch can put a picture on screen
//! without resuming.
//!
//! # Structure, as this decoder reads it
//!
//! ```text
//! [packet size u16be][control offset u16be][ ... RLE pixel data ... ][ DCSQ ]
//! ```
//!
//! The header's second field points at the first control block. Each block is
//!
//! ```text
//! [delay u16be][next block offset u16be][ commands ... 0xff ]
//! ```
//!
//! and the chain ends when a block points at itself. `delay` is in units of
//! 1024/90000 s from the packet's own presentation time, which is what turns
//! the chain into a schedule: the block carrying DSP is when the subtitle
//! appears, the block carrying STP_DSP is when it goes away, and a block
//! carrying CHG_COLCON in between is a recolouring of the picture already up.
//!
//! Eight commands, all of them handled: FSTA_DSP (forced display, parsed and
//! dropped, since surfacing "forced" is receiver UX of its own), DSP,
//! STP_DSP, SET_COLOR and SET_ALPHA (four 4-bit palette indices each, packed
//! two to a byte), SET_DAREA (the display rectangle, 12-bit coordinates packed
//! across seven bytes), DSPXA (byte offsets of the two interlaced fields of
//! pixel data) and CHG_COLCON (per-scanline palette overrides).
//!
//! # The palette arrives OUT OF BAND, and it is RGB
//!
//! A DVD player gets its 16-entry palette from the disc, on a side channel; in
//! matroska it arrives as CodecPrivate (the `.idx` text) and reaches this
//! decoder through [`SubpicDecoder::set_codec_data`]. The four indices in
//! SET_COLOR select from it.
//!
//! **Those entries are RGB, not YCbCr, and this is the one place where the
//! reference could not help**: `gstspu-vobsub-render.c` only ever sees the
//! DVD's own CLUT, which IS YCbCr (`0xYYVVUU`, and the reference's comment
//! about V and U being stored the other way round is the giveaway). The `.idx`
//! path does not exist there at all. Checked against the real sample rather
//! than assumed: its palette contains `101010, a1a1a1, c5c5c5, ebebeb`, which
//! is a grey ramp in RGB and four saturated colours in YCbCr, and its
//! remaining entries (`0d00ee` blue, `ee450d` orange, `0ce60b` green) are
//! subtitle colours in RGB and noise in YCbCr. ffmpeg's `dvdsubdec` reads the
//! same line the same way.
//!
//! # Timing
//!
//! `start_rt` is the packet's running time plus the DSP block's delay; `end_rt`
//! is the packet's running time plus the STP_DSP block's delay. A packet whose
//! schedule never stops falls back to the buffer's duration (matroska's
//! BlockDuration, which the driver forwards) and, failing that, stays
//! open-ended until something supersedes it.
//!
//! # Geometry
//!
//! SET_DAREA is in the authoring grid's coordinates, a DVD frame 720 wide.
//! The `.idx`'s `size:` line names that grid when the container supplies one;
//! failing that this decoder assumes 720x480 and widens to 720x576 if the
//! rectangle needs it. The mapping onto the video is a STRETCH, not a fit
//! a DVD subtitle is authored against a 4:3 or anamorphic frame
//! whose pixels are not square, so preserving the authoring aspect would put
//! the subtitle in the wrong place on precisely the anamorphic content it was
//! made for.
//!
//! # Provenance
//!
//! Written fresh against the format's structure, with `gstspu-vobsub.c` and
//! `gstspu-vobsub-render.c` (LGPL) consulted as NORMATIVE REFERENCES for
//! behaviour the structure does not fix. Every consultation is named at the
//! site that made the decision: the command lengths and their bit packing, the
//! DCSQ chain's self-pointing terminator, the nibble RLE's four-step widening,
//! `run == 0` meaning "to the end of the line", the byte alignment at the start
//! of each line, which field the first line comes from, the CHG_COLCON entry
//! layout and its `0x0fffffff` terminator, and the guessed grey ramp when no
//! palette ever arrives. Nothing is transliterated: the state machine, the
//! error policy, the accounting and the output model (regions to a compositor,
//! straight alpha, one update per visual state) are this crate's.
//!
//! **The reference premultiplies its palette by alpha**
//! (`gstspu-vobsub-render.c:62-64`) and this must not, for the same reason PGS
//! must not: the renderer composites straight alpha. The canary test pins it
//! here too.
//!
//! # Hostile input
//!
//! Stream bytes are untrusted: every malformed structure is a counted, logged
//! reset, nothing panics, and every allocation is charged against the same
//! 32 MiB per-decoder budget the other decoders use, before it is made.

use std::sync::Arc;

use tracing::{debug, warn};

use super::{
    BitmapPacket, BitmapRegion, DisplayUpdate, SubpicDecoder,
    pgs::{ALLOCATION_BUDGET, Rgba},
};

/// Control commands, from the format's own table.
const CMD_FSTA_DSP: u8 = 0x00;
const CMD_DSP: u8 = 0x01;
const CMD_STP_DSP: u8 = 0x02;
const CMD_SET_COLOR: u8 = 0x03;
const CMD_SET_ALPHA: u8 = 0x04;
const CMD_SET_DAREA: u8 = 0x05;
const CMD_DSPXA: u8 = 0x06;
const CMD_CHG_COLCON: u8 = 0x07;
const CMD_END: u8 = 0xFF;

/// `[packet size u16be][control offset u16be]`.
const PACKET_HEADER: usize = 4;

/// A control block's own header: `[delay u16be][next offset u16be]`.
const BLOCK_HEADER: usize = 4;

/// Delays are in 1024/90000 s.
const DELAY_NUMERATOR: u64 = 1024 * 1_000_000_000;
const DELAY_DENOMINATOR: u64 = 90_000;

/// The authoring grid this decoder assumes when the container names none.
const DEFAULT_GRID: (u32, u32) = (720, 480);
/// ...widened to this when the display rectangle does not fit the default. A
/// PAL disc is 720x576 and an NTSC one 720x480, and a rectangle that runs past
/// 480 rows can only have come from the taller of the two.
const TALL_GRID_HEIGHT: u32 = 576;

/// How many per-scanline colour overrides one packet may declare. The format
/// allows a change per line; this is two full PAL frames' worth and bounds the
/// only structure the decoder keeps that a packet can grow.
const MAX_LINE_OVERRIDES: usize = 1200;

/// Why a packet is being thrown away. Mirrors `pgs::Fault` deliberately: two
/// decoders, one error policy, and the budget's own reason distinguished from
/// a corrupt stream so a test can tell them apart.
#[derive(Debug, Clone, Copy)]
enum Fault {
    Malformed(&'static str),
    Budget(&'static str),
}

impl Fault {
    fn reason(self) -> &'static str {
        match self {
            Self::Malformed(reason) | Self::Budget(reason) => reason,
        }
    }
}

/// A display rectangle in the authoring grid, with INCLUSIVE bounds, which is
/// the format's own convention (`right - left + 1` is the width) and the source
/// of every off-by-one this format is famous for.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct Rect {
    left: u32,
    top: u32,
    right: u32,
    bottom: u32,
}

impl Rect {
    fn width(&self) -> u32 {
        self.right.saturating_sub(self.left) + 1
    }

    fn height(&self) -> u32 {
        self.bottom.saturating_sub(self.top) + 1
    }

    fn is_empty(&self) -> bool {
        self.right < self.left || self.bottom < self.top
    }
}

/// One CHG_COLCON entry: a band of scanlines, and the palette changes that
/// apply across it.
#[derive(Debug, Clone)]
struct LineOverride {
    top: u32,
    bottom: u32,
    /// `(x from which this applies, the four (index, alpha) pairs)`, in
    /// left-to-right order.
    changes: Vec<(u32, [u8; 4], [u8; 4])>,
}

/// The four-entry palette a subpicture draws with: indices into the 16-entry
/// CLUT plus 4-bit alphas, both set by their own command.
/// Nothing selected and nothing visible by default: a packet that never sets a
/// palette draws nothing rather than drawing garbage.
#[derive(Debug, Clone, Copy, Default)]
struct SubPalette {
    index: [u8; 4],
    alpha: [u8; 4],
}

/// The decoder's understanding of the picture at one point in a packet's
/// schedule.
#[derive(Debug, Clone, Default)]
struct DisplayState {
    palette: SubPalette,
    rect: Rect,
    /// Byte offsets of the two interlaced fields, from the start of the packet.
    fields: [usize; 2],
    overrides: Vec<LineOverride>,
    forced: bool,
}

/// The VOBSUB decoder: one packet in, that packet's whole schedule out.
pub struct VobsubDecoder {
    /// The 16-entry CLUT from the `.idx`, already RGB. `None` until the
    /// container supplies one (see the module docs for why it is not YCbCr).
    clut: Option<[Rgba; 16]>,
    /// The authoring grid from the `.idx`'s `size:` line, when it gave one.
    grid: Option<(u32, u32)>,
    video_size: Option<(u32, u32)>,
    errors: u64,
    budget_resets: u64,
    failing: bool,
}

impl Default for VobsubDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl VobsubDecoder {
    pub fn new() -> Self {
        Self {
            clut: None,
            grid: None,
            video_size: None,
            errors: 0,
            budget_resets: 0,
            failing: false,
        }
    }

    /// Allocation-budget breaches this decoder has counted, cumulative.
    pub fn budget_resets(&self) -> u64 {
        self.budget_resets
    }

    /// What this decoder is holding, in bytes charged against
    /// [`ALLOCATION_BUDGET`].
    ///
    /// Almost nothing, and that is the format: a subpicture unit is
    /// self-contained, so between packets there is a 16-entry palette and a
    /// grid size and no pixel data at all. The budget still governs, at the one
    /// place this decoder allocates: the page a packet expands into, priced
    /// from its rectangle before a byte of it is taken.
    pub fn held_bytes(&self) -> u64 {
        (self.clut.map_or(0, |clut| std::mem::size_of_val(&clut))) as u64
    }

    /// What this decoder's buffers have actually taken from the allocator.
    ///
    /// The same standing invariant the other decoders carry (`allocated_bytes()
    /// <= held_bytes()`), and here it is exact by construction: the palette is
    /// an array inside the struct, not a heap allocation, so both numbers
    /// describe the same fixed bytes.
    #[doc(hidden)]
    pub fn allocated_bytes(&self) -> u64 {
        self.held_bytes()
    }

    fn fail(&mut self, fault: Fault) {
        self.errors += 1;
        if matches!(fault, Fault::Budget(_)) {
            self.budget_resets += 1;
        }
        let reason = fault.reason();
        if self.failing {
            debug!(reason, "vobsub: dropping the packet");
        } else {
            warn!(
                reason,
                "vobsub: dropping the packet; this line is not repeated until one decodes"
            );
            self.failing = true;
        }
        // NOTHING to drop. A subpicture unit is self-contained, so a packet
        // that fails takes nothing with it and the next one is unaffected,
        // this format cannot cascade, which is the whole of its error recovery.
        // The palette and the grid come from the CONTAINER and survive: they
        // are not what the broken packet said.
    }

    /// The authoring grid this packet's coordinates are in.
    ///
    /// The container's `size:` when it gave one. Failing that the NTSC frame,
    /// widened to PAL when the rectangle needs the rows. A rectangle running
    /// past row 480 can only have come from the taller disc, and guessing the
    /// short grid there would push the subtitle off the bottom of the video.
    fn grid_for(&self, rect: &Rect) -> (u32, u32) {
        if let Some(grid) = self.grid {
            return grid;
        }
        let (width, height) = DEFAULT_GRID;
        if rect.bottom >= height {
            (width, TALL_GRID_HEIGHT)
        } else {
            (width, height)
        }
    }

    /// Expand one of the four drawing colours to straight-alpha RGBA.
    ///
    /// Two sources, and the second is the reference's: with a palette, the CLUT
    /// entry and the 4-bit alpha widened to 8 (`a << 4 | a`, so 0xF is fully
    /// opaque and not 0xF0). Without one (no container ever supplied a
    /// `.idx`) the reference guesses a ramp of white, grey and black for the
    /// visible entries rather than drawing nothing, which is the difference
    /// between unreadable subtitles and no subtitles.
    ///
    /// **STRAIGHT ALPHA.** The reference premultiplies here
    /// (`gstspu-vobsub-render.c:62-64`); this renderer composites with
    /// independent alpha and a premultiplied palette would wash out every
    /// anti-aliased edge.
    fn colour(&self, palette: &SubPalette, entry: usize) -> Rgba {
        let alpha4 = palette.alpha[entry] & 0x0F;
        let alpha = (alpha4 << 4) | alpha4;
        match self.clut {
            Some(clut) => {
                let [r, g, b, _] = clut[usize::from(palette.index[entry] & 0x0F)];
                [r, g, b, alpha]
            }
            None => {
                if alpha == 0 {
                    return [0, 0, 0, 0];
                }
                // The ramp is by VISIBLE entry, exactly as the reference walks
                // it: the first visible colour is white, the next grey, the
                // rest black.
                let visible_before = (0..entry).filter(|&i| palette.alpha[i] & 0x0F != 0).count();
                let level = 255u32.saturating_sub(128 * visible_before as u32) as u8;
                [level, level, level, alpha]
            }
        }
    }
}

impl SubpicDecoder for VobsubDecoder {
    /// The `.idx` text from the container's CodecPrivate.
    ///
    /// Two lines matter, `palette:` (sixteen RGB triples in hex) and `size:`
    /// (the authoring grid), and everything else in the file is a player's
    /// business rather than a decoder's: `forced subs:`, `org:`, the
    /// per-language index. Unparsable input leaves the previous palette in
    /// place rather than clearing it: a decoder with a stale palette draws the
    /// wrong colours, a decoder with no palette draws a grey ramp, and of the
    /// two the first is much more likely to be right.
    fn set_codec_data(&mut self, data: &[u8]) {
        let Ok(text) = std::str::from_utf8(data) else {
            warn!("vobsub: codec_data is not text; keeping the palette we have");
            return;
        };
        for line in text.lines() {
            let line = line.trim();
            if let Some(rest) = line.strip_prefix("palette:") {
                let mut clut = [[0u8; 4]; 16];
                let mut seen = 0usize;
                for entry in rest.split(',') {
                    if seen == clut.len() {
                        break;
                    }
                    let Ok(value) = u32::from_str_radix(entry.trim(), 16) else {
                        continue;
                    };
                    // RGB, opaque; the alpha in a subpicture comes from
                    // SET_ALPHA, never from the palette.
                    clut[seen] = [
                        ((value >> 16) & 0xFF) as u8,
                        ((value >> 8) & 0xFF) as u8,
                        (value & 0xFF) as u8,
                        0xFF,
                    ];
                    seen += 1;
                }
                if seen == 0 {
                    debug!("vobsub: a palette line with no entries in it");
                    continue;
                }
                debug!(entries = seen, "vobsub: palette from the container");
                self.clut = Some(clut);
            } else if let Some(rest) = line.strip_prefix("size:") {
                let mut parts = rest.trim().split(['x', 'X']);
                if let (Some(Ok(width)), Some(Ok(height))) = (
                    parts.next().map(|w| w.trim().parse::<u32>()),
                    parts.next().map(|h| h.trim().parse::<u32>()),
                ) && width > 0
                    && height > 0
                {
                    debug!(width, height, "vobsub: authoring grid from the container");
                    self.grid = Some((width, height));
                }
            }
        }
    }

    fn set_video_size(&mut self, width: u32, height: u32) {
        self.video_size = Some((width, height));
    }

    fn push(&mut self, packet: &BitmapPacket) -> Vec<DisplayUpdate> {
        let Ok(map) = packet.data.map_readable() else {
            self.fail(Fault::Malformed("a packet that could not be mapped"));
            return Vec::new();
        };
        match self.decode(map.as_slice(), packet.rt, packet.duration) {
            Ok(updates) => {
                if !updates.is_empty() {
                    self.failing = false;
                }
                updates
            }
            Err(fault) => {
                self.fail(fault);
                Vec::new()
            }
        }
    }

    /// Back to just-constructed, the container's teachings included: the engine
    /// re-teaches both before the next packet.
    fn reset(&mut self) {
        self.clut = None;
        self.grid = None;
        self.video_size = None;
        self.failing = false;
    }

    fn take_decode_errors(&mut self) -> u64 {
        std::mem::take(&mut self.errors)
    }
}

impl VobsubDecoder {
    /// One subpicture unit, from its header to the end of its schedule.
    fn decode(
        &mut self,
        data: &[u8],
        rt: gst::ClockTime,
        duration: Option<gst::ClockTime>,
    ) -> Result<Vec<DisplayUpdate>, Fault> {
        // A TERMINATOR, not a packet. Matroska and the DVD both emit short
        // buffers to close a subtitle whose SPU never said when to stop; the
        // reference refuses anything under four bytes as invalid, and this
        // treats the two-byte case as what it is (a delivery with no picture
        // in it) rather than counting it against the stream.
        if data.len() <= 2 {
            debug!(len = data.len(), "vobsub: a terminator, not a subpicture");
            return Ok(Vec::new());
        }
        if data.len() < PACKET_HEADER {
            return Err(Fault::Malformed("a packet shorter than its own header"));
        }

        // The packet's own size field: authoritative where it is shorter than
        // the buffer (a container may pad), refused where it is longer, since
        // every offset inside the packet is relative to this length.
        let declared = usize::from(be16(&data[0..2]));
        let data = match declared {
            // A container that padded and a packet that did not bother to say
            // its own size both end up here; the buffer's length is the truth.
            0 => data,
            // SHORTER THAN ITS OWN HEADER. Found by the fuzz target in its
            // first campaign: the length check above is on the BUFFER, and
            // re-slicing to a declared 1, 2 or 3 bytes then read a header that
            // was no longer there, a panic on four bytes of input, which is
            // exactly the class the cap exists to prevent.
            declared if declared < PACKET_HEADER => {
                return Err(Fault::Malformed(
                    "a packet declaring less than its own header",
                ));
            }
            declared if declared <= data.len() => &data[..declared],
            _ => return Err(Fault::Malformed("a packet claiming more bytes than it has")),
        };

        let mut at = usize::from(be16(&data[2..4]));
        let mut state = DisplayState::default();
        let mut updates: Vec<DisplayUpdate> = Vec::new();
        // What this packet has handed back so far; see `render`.
        let mut produced = 0u64;
        // The block that turned the picture on, and the picture it described.
        let mut showing: Option<(gst::ClockTime, DisplayState)> = None;
        let mut blocks = 0usize;
        // The latest time any block has been given: a schedule cannot go
        // backwards (see the clamp below).
        let mut watermark = rt;

        loop {
            if at + BLOCK_HEADER > data.len() {
                return Err(Fault::Malformed(
                    "a control block past the end of its packet",
                ));
            }
            // A packet cannot have more control blocks than it has room for,
            // and a chain that says otherwise is a loop.
            blocks += 1;
            if blocks > data.len() / BLOCK_HEADER + 1 {
                return Err(Fault::Malformed("a control chain that never ends"));
            }

            let delay = u64::from(be16(&data[at..at + 2]));
            let next = usize::from(be16(&data[at + 2..at + 4]));
            let block_rt = rt
                .checked_add(gst::ClockTime::from_nseconds(
                    delay * DELAY_NUMERATOR / DELAY_DENOMINATOR,
                ))
                .ok_or(Fault::Malformed("a control block past the end of time"))?;
            // THE CHAIN IS A SCHEDULE, so it only runs forwards. Nothing in the
            // format stops a packet from pointing at a block whose delay is
            // smaller than the one before it, and a hostile one does: found by
            // the fuzz target, as an update that ended a minute before it
            // started. A block that is already due when it is reached happens
            // NOW, which is what the reference's timing engine does with one
            // too, having no way to run it any earlier.
            let block_rt = block_rt.max(watermark);
            watermark = block_rt;

            let outcome = self.run_block(&data[at + BLOCK_HEADER..], &mut state)?;

            match outcome {
                // The picture goes away at this block's time. If one was up,
                // this is its end; nothing new is drawn.
                BlockOutcome::Stop => {
                    if let Some(update) = updates.last_mut() {
                        update.end_rt = Some(block_rt);
                    }
                    showing = None;
                }
                BlockOutcome::Show | BlockOutcome::Change => {
                    // A CHANGE only matters while something is on screen: the
                    // format lets a block recolour a picture that is already
                    // up, and that is a new update superseding the old one at
                    // the moment it takes effect.
                    let already = matches!(outcome, BlockOutcome::Change);
                    if already && showing.is_none() {
                        // Not shown yet: fold the change into the state and let
                        // the DSP block that follows draw it.
                    } else {
                        if let Some(update) = updates.last_mut() {
                            update.end_rt = Some(block_rt);
                        }
                        let regions = self.render(data, &state, &mut produced)?;
                        showing = Some((block_rt, state.clone()));
                        updates.push(DisplayUpdate {
                            start_rt: block_rt,
                            end_rt: None,
                            regions,
                        });
                    }
                }
                BlockOutcome::Quiet => {}
            }

            if next == at || next == 0 {
                break;
            }
            at = next;
        }

        if updates.is_empty() {
            debug!(%rt, "vobsub: a packet whose schedule never showed anything");
            return Ok(updates);
        }

        // THE FALLBACK END. A schedule that never stops is legal and common:
        // the container is expected to say how long the block lasts, which
        // matroska does with BlockDuration and the driver forwards. Failing
        // both, the picture is open-ended and only the next subtitle takes it
        // away, which is the same contract PGS has, and is what a terminator
        // packet exists to end.
        if let Some(last) = updates.last_mut()
            && last.end_rt.is_none()
            && let Some(duration) = duration
        {
            last.end_rt = rt.checked_add(duration);
        }
        let _ = showing;
        Ok(updates)
    }

    /// Run one control block's commands against `state`, and say what the block
    /// did to the picture.
    fn run_block(&self, block: &[u8], state: &mut DisplayState) -> Result<BlockOutcome, Fault> {
        let mut at = 0usize;
        let mut outcome = BlockOutcome::Quiet;
        while at < block.len() {
            let command = block[at];
            // The command lengths are the format's, and the reference is where
            // the packing of the two-byte and seven-byte ones is written down.
            match command {
                CMD_FSTA_DSP => {
                    // Parsed and carried no further.
                    state.forced = true;
                    outcome = outcome.at_least(BlockOutcome::Show);
                    at += 1;
                }
                CMD_DSP => {
                    outcome = outcome.at_least(BlockOutcome::Show);
                    at += 1;
                }
                CMD_STP_DSP => {
                    outcome = BlockOutcome::Stop;
                    at += 1;
                }
                CMD_SET_COLOR => {
                    let field = read(block, at + 1, 2)?;
                    state.palette.index = [
                        field[1] & 0x0F,
                        field[1] >> 4,
                        field[0] & 0x0F,
                        field[0] >> 4,
                    ];
                    outcome = outcome.at_least(BlockOutcome::Change);
                    at += 3;
                }
                CMD_SET_ALPHA => {
                    let field = read(block, at + 1, 2)?;
                    state.palette.alpha = [
                        field[1] & 0x0F,
                        field[1] >> 4,
                        field[0] & 0x0F,
                        field[0] >> 4,
                    ];
                    outcome = outcome.at_least(BlockOutcome::Change);
                    at += 3;
                }
                CMD_SET_DAREA => {
                    let field = read(block, at + 1, 6)?;
                    // Six bytes carrying four 12-bit coordinates, packed
                    // left/right and top/bottom around the shared nibbles of
                    // bytes 1 and 4.
                    state.rect = Rect {
                        left: (u32::from(field[0]) << 4) | (u32::from(field[1]) >> 4),
                        right: ((u32::from(field[1]) & 0x0F) << 8) | u32::from(field[2]),
                        top: (u32::from(field[3]) << 4) | (u32::from(field[4]) >> 4),
                        bottom: ((u32::from(field[4]) & 0x0F) << 8) | u32::from(field[5]),
                    };
                    outcome = outcome.at_least(BlockOutcome::Change);
                    at += 7;
                }
                CMD_DSPXA => {
                    let field = read(block, at + 1, 4)?;
                    state.fields = [
                        usize::from(be16(&field[0..2])),
                        usize::from(be16(&field[2..4])),
                    ];
                    outcome = outcome.at_least(BlockOutcome::Change);
                    at += 5;
                }
                CMD_CHG_COLCON => {
                    let size = usize::from(be16(read(block, at + 1, 2)?));
                    if size < 2 || at + 1 + size > block.len() {
                        return Err(Fault::Malformed(
                            "a colour-change command past the end of its block",
                        ));
                    }
                    state.overrides = parse_line_overrides(&block[at + 3..at + 1 + size]);
                    outcome = outcome.at_least(BlockOutcome::Change);
                    // The size counts from its own first byte, which is why the
                    // command advances by `1 + size` and not by `3 + size`.
                    at += 1 + size;
                }
                CMD_END => break,
                other => {
                    // The reference stops the block on anything it does not
                    // know, and so does this: command lengths are per-command,
                    // so an unknown one means every byte after it is
                    // unreadable.
                    debug!(command = other, "vobsub: an unknown control command");
                    break;
                }
            }
        }
        Ok(outcome)
    }

    /// Expand the packet's two interlaced fields into one region.
    ///
    /// `produced` is what this PACKET has already handed back, and it is part
    /// of the price. The cap this decoder had for two steps was on what it
    /// RETAINS, which a self-contained format keeps small by construction:
    /// but one packet's command schedule may show, recolour and re-show, and
    /// every one of those is a fresh picture in the vector this call returns.
    /// A 4 MiB picture shown two hundred times in one schedule is 800 MiB the
    /// caller is handed at once while `held_bytes()` reports almost nothing.
    /// Found by the exit gate's own hour-long campaign (2.19 GB live in 194
    /// allocations, all of them from here), which is exactly the shape the cap
    /// exists to forbid and the shape a retention-only cap cannot see.
    fn render(
        &self,
        packet: &[u8],
        state: &DisplayState,
        produced: &mut u64,
    ) -> Result<Vec<BitmapRegion>, Fault> {
        if state.rect.is_empty() {
            debug!("vobsub: a picture with an empty display area");
            return Ok(Vec::new());
        }
        let (width, height) = (state.rect.width(), state.rect.height());

        // THE ALLOCATION SITE, priced before a byte is taken, against what
        // this decoder holds AND what this packet has already produced.
        let bytes = u64::from(width) * u64::from(height) * 4;
        if bytes + *produced + self.held_bytes() > ALLOCATION_BUDGET {
            return Err(Fault::Budget(
                "a picture past the decoder's allocation budget",
            ));
        }
        *produced += bytes;
        let mut pixels = vec![0u8; (width * height * 4) as usize];

        // Four colours, expanded once for the whole picture.
        let palette: [Rgba; 4] = std::array::from_fn(|entry| self.colour(&state.palette, entry));

        // TWO FIELDS, INTERLACED BY LINE, and the first line of the rectangle
        // always comes from the first field. (The reference swaps the two when
        // the rectangle starts on an odd row, which is the same statement made
        // from the frame's parity instead of the rectangle's.)
        let mut offsets = [state.fields[0] * 2, state.fields[1] * 2];
        for row in 0..height {
            let field = (row & 1) as usize;
            // Every line starts byte-aligned; a line that ended mid-byte pads.
            offsets[field] = offsets[field].div_ceil(2) * 2;
            let overrides = state
                .overrides
                .iter()
                .find(|entry| {
                    let line = state.rect.top + row;
                    line >= entry.top && line <= entry.bottom
                })
                .cloned();
            self.render_line(
                packet,
                &mut offsets[field],
                &palette,
                overrides.as_ref(),
                state,
                &mut pixels,
                row,
                width,
            );
        }

        let Some(region) = self.place(state, width, height, pixels) else {
            return Ok(Vec::new());
        };
        Ok(vec![region])
    }

    /// One scanline of nibble RLE into `pixels`.
    #[allow(clippy::too_many_arguments)]
    fn render_line(
        &self,
        packet: &[u8],
        offset: &mut usize,
        palette: &[Rgba; 4],
        overrides: Option<&LineOverride>,
        state: &DisplayState,
        pixels: &mut [u8],
        row: u32,
        width: u32,
    ) {
        let mut x = 0u32;
        while x < width {
            let code = read_rle_code(packet, offset);
            let entry = (code & 3) as usize;
            // A run length of zero means "to the end of the line", which is how
            // a subtitle's trailing transparency costs two nibbles instead of
            // four hundred.
            let run = match code >> 2 {
                0 => width - x,
                run => run.min(width - x),
            };

            // A RUN CAN CROSS A COLOUR CHANGE, and then it is two runs of
            // different colours: the reference splits at the region boundary
            // for the same reason (`gstspu-vobsub-render.c`'s
            // render_line_with_chgcol walks `cur_reg_end` inside the run). A
            // decoder that colours the whole run from its first pixel paints
            // over exactly the highlight the change exists to draw.
            let mut painted = 0u32;
            while painted < run {
                let at_x = x + painted;
                let absolute = state.rect.left + at_x;
                let (colour, next_change) = match overrides {
                    // The overrides are in the AUTHORING grid's x, like the
                    // rectangle they sit in.
                    Some(entry_override) => {
                        let active = entry_override
                            .changes
                            .iter()
                            .rev()
                            .find(|(from, ..)| *from <= absolute);
                        let next = entry_override
                            .changes
                            .iter()
                            .find(|(from, ..)| *from > absolute)
                            .map(|(from, ..)| from.saturating_sub(state.rect.left));
                        let colour = match active {
                            Some((_, index, alpha)) => self.colour(
                                &SubPalette {
                                    index: *index,
                                    alpha: *alpha,
                                },
                                entry,
                            ),
                            None => palette[entry],
                        };
                        (colour, next)
                    }
                    None => (palette[entry], None),
                };
                let span = match next_change {
                    Some(boundary) if boundary > at_x => (boundary - at_x).min(run - painted),
                    _ => run - painted,
                };
                let start = ((row * width + at_x) * 4) as usize;
                for pixel in 0..span as usize {
                    let at = start + pixel * 4;
                    pixels[at..at + 4].copy_from_slice(&colour);
                }
                painted += span.max(1);
            }
            x += run;
        }
    }

    /// Put the picture on the video: the authoring grid STRETCHED to the coded
    /// size, with the rect clipped to the picture.
    fn place(
        &self,
        state: &DisplayState,
        width: u32,
        height: u32,
        pixels: Vec<u8>,
    ) -> Option<BitmapRegion> {
        let (grid_width, grid_height) = self.grid_for(&state.rect);
        let (video_width, video_height) = self.video_size.unwrap_or((grid_width, grid_height));

        let scale_x = f64::from(video_width) / f64::from(grid_width);
        let scale_y = f64::from(video_height) / f64::from(grid_height);
        let x = (f64::from(state.rect.left) * scale_x).round() as i32;
        let y = (f64::from(state.rect.top) * scale_y).round() as i32;
        if x >= video_width as i32 || y >= video_height as i32 {
            debug!(x, y, "vobsub: a picture placed off the video");
            return None;
        }
        // Contained, not merely started inside a
        // rectangle authored for a wider grid than the video must not hang off
        // the edge for a compositor to deal with.
        let render_width = ((f64::from(width) * scale_x).round() as u32)
            .min(video_width - x as u32)
            .max(1);
        let render_height = ((f64::from(height) * scale_y).round() as u32)
            .min(video_height - y as u32)
            .max(1);

        Some(BitmapRegion {
            pixels: Arc::new(pixels),
            width,
            height,
            x,
            y,
            render_width,
            render_height,
        })
    }
}

/// What a control block did to the picture. Ordered: a block that both
/// recolours and shows is a Show.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum BlockOutcome {
    Quiet,
    Change,
    Show,
    Stop,
}

impl BlockOutcome {
    fn at_least(self, other: Self) -> Self {
        // Stop wins outright: a block that sets a palette and then turns the
        // display off has turned the display off.
        if self == Self::Stop {
            self
        } else {
            self.max(other)
        }
    }
}

/// `n` bytes at `at`, or a malformed-packet fault.
fn read(block: &[u8], at: usize, n: usize) -> Result<&[u8], Fault> {
    block.get(at..at + n).ok_or(Fault::Malformed(
        "a control command past the end of its block",
    ))
}

/// CHG_COLCON's per-scanline entries.
///
/// Each is a band (`top`, `bottom`, both 10-bit and packed with the change
/// count) followed by up to eight six-byte changes: an x to apply from, and a
/// 32-bit word holding four palette indices and four alphas. The list ends at
/// `0x0fffffff` or when it runs out of bytes, both the reference's rules.
fn parse_line_overrides(mut data: &[u8]) -> Vec<LineOverride> {
    let mut out = Vec::new();
    while data.len() >= 4 && out.len() < MAX_LINE_OVERRIDES {
        if be32(&data[0..4]) == 0x0FFF_FFFF {
            break;
        }
        let top = ((u32::from(data[0]) << 8) & 0x300) | u32::from(data[1]);
        let bottom = ((u32::from(data[2]) << 8) & 0x300) | u32::from(data[3]);
        let count = usize::from(data[2] >> 4).clamp(1, 8);
        let Some(body) = data.get(4..4 + count * 6) else {
            break;
        };
        let mut changes = Vec::with_capacity(count);
        for change in body.chunks(6) {
            let left = ((u32::from(change[0]) << 8) & 0x300) | u32::from(change[1]);
            let word = be32(&change[2..6]);
            changes.push((
                left,
                [
                    ((word >> 16) & 0x0F) as u8,
                    ((word >> 20) & 0x0F) as u8,
                    ((word >> 24) & 0x0F) as u8,
                    ((word >> 28) & 0x0F) as u8,
                ],
                [
                    (word & 0x0F) as u8,
                    ((word >> 4) & 0x0F) as u8,
                    ((word >> 8) & 0x0F) as u8,
                    ((word >> 12) & 0x0F) as u8,
                ],
            ));
        }
        out.push(LineOverride {
            top,
            bottom,
            changes,
        });
        data = &data[4 + count * 6..];
    }
    out
}

/// One RLE code, in nibbles.
///
/// The widening is the format's: a nibble whose value is 4 or more is a
/// complete code; below that it takes the next nibble, and so on to four
/// nibbles. The low two bits are the colour, the rest is the run.
fn read_rle_code(packet: &[u8], offset: &mut usize) -> u32 {
    let mut code = u32::from(read_nibble(packet, offset));
    if code < 0x4 {
        code = (code << 4) | u32::from(read_nibble(packet, offset));
        if code < 0x10 {
            code = (code << 4) | u32::from(read_nibble(packet, offset));
            if code < 0x40 {
                code = (code << 4) | u32::from(read_nibble(packet, offset));
            }
        }
    }
    code
}

/// One nibble at a nibble offset, high half first. Past the end reads as zero,
/// which is the reference's behaviour and means a truncated picture finishes
/// its lines transparently instead of failing.
fn read_nibble(packet: &[u8], offset: &mut usize) -> u8 {
    let byte = packet.get(*offset / 2).copied().unwrap_or(0);
    let nibble = if *offset & 1 == 1 {
        byte & 0x0F
    } else {
        byte >> 4
    };
    *offset += 1;
    nibble
}

fn be16(bytes: &[u8]) -> u16 {
    u16::from_be_bytes([bytes[0], bytes[1]])
}

fn be32(bytes: &[u8]) -> u32 {
    u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

/// Hand-crafted subpicture units, and the real `.idx` from the sample.
///
/// Doc-hidden and shipped for the same reason [`super::pgs::fixtures`] is:
/// three places need the same bytes (this file's vectors, the fuzz target's
/// known-good tail, and its seed corpus) and a `cfg(test)` module is invisible
/// to both `tests/` and `fuzz/`.
#[doc(hidden)]
pub mod fixtures {
    use super::{
        BLOCK_HEADER, CMD_CHG_COLCON, CMD_DSP, CMD_DSPXA, CMD_END, CMD_SET_ALPHA, CMD_SET_COLOR,
        CMD_SET_DAREA, CMD_STP_DSP, PACKET_HEADER,
    };

    /// The `.idx` CodecPrivate of
    /// `fcast-sample-media/video/video_with_vobsub.mkv`, byte for byte: 167
    /// bytes, a `size:` line and sixteen RGB palette entries. Read out of
    /// the container rather than typed from memory.
    pub const SAMPLE_IDX: &[u8] = b"size: 720x480\npalette: 0d00ee, ee450d, 101010, ebebeb, 0ce60b, ec14ed, ebff0b, 0d637e, a1a1a1, c5c5c5, 0e640c, 89db89, 0e0089, a2bdd4, ebcf0b, 7e127e\nforced subs: OFF\n";

    /// One control block: a delay, the offset of the next block, and commands.
    pub struct Block {
        pub delay: u16,
        pub commands: Vec<u8>,
    }

    pub fn display_on() -> Vec<u8> {
        vec![CMD_DSP]
    }

    pub fn display_off() -> Vec<u8> {
        vec![CMD_STP_DSP]
    }

    /// `(background, pattern, emphasis 1, emphasis 2)` palette indices.
    pub fn set_color(entries: [u8; 4]) -> Vec<u8> {
        vec![
            CMD_SET_COLOR,
            (entries[3] << 4) | (entries[2] & 0x0F),
            (entries[1] << 4) | (entries[0] & 0x0F),
        ]
    }

    /// The same four entries' 4-bit alphas.
    pub fn set_alpha(entries: [u8; 4]) -> Vec<u8> {
        vec![
            CMD_SET_ALPHA,
            (entries[3] << 4) | (entries[2] & 0x0F),
            (entries[1] << 4) | (entries[0] & 0x0F),
        ]
    }

    /// The display rectangle, in the authoring grid, INCLUSIVE.
    pub fn set_area(left: u32, top: u32, right: u32, bottom: u32) -> Vec<u8> {
        vec![
            CMD_SET_DAREA,
            (left >> 4) as u8,
            (((left & 0x0F) << 4) | (right >> 8)) as u8,
            (right & 0xFF) as u8,
            (top >> 4) as u8,
            (((top & 0x0F) << 4) | (bottom >> 8)) as u8,
            (bottom & 0xFF) as u8,
        ]
    }

    /// Byte offsets of the two interlaced fields.
    pub fn set_fields(top: u16, bottom: u16) -> Vec<u8> {
        let mut out = vec![CMD_DSPXA];
        out.extend_from_slice(&top.to_be_bytes());
        out.extend_from_slice(&bottom.to_be_bytes());
        out
    }

    /// A colour-change command from ready-made entry bodies.
    pub fn change_colour(entries: &[Vec<u8>]) -> Vec<u8> {
        let mut body: Vec<u8> = entries.concat();
        body.extend_from_slice(&0x0FFF_FFFFu32.to_be_bytes());
        let size = (body.len() + 2) as u16;
        let mut out = vec![CMD_CHG_COLCON];
        out.extend_from_slice(&size.to_be_bytes());
        out.extend_from_slice(&body);
        out
    }

    /// One colour-change entry: a band of lines, and one change from `left`.
    pub fn change_entry(
        top: u32,
        bottom: u32,
        left: u32,
        index: [u8; 4],
        alpha: [u8; 4],
    ) -> Vec<u8> {
        let word = (u32::from(index[3]) << 28)
            | (u32::from(index[2]) << 24)
            | (u32::from(index[1]) << 20)
            | (u32::from(index[0]) << 16)
            | (u32::from(alpha[3]) << 12)
            | (u32::from(alpha[2]) << 8)
            | (u32::from(alpha[1]) << 4)
            | u32::from(alpha[0]);
        // The change count lives in the high nibble of byte 2, which is the
        // one packing the format does that a reader has to be told about.
        let mut out = vec![
            ((top >> 8) & 0x03) as u8,
            (top & 0xFF) as u8,
            ((((bottom >> 8) & 0x03) as u8) | (1 << 4)),
            (bottom & 0xFF) as u8,
        ];
        out.push(((left >> 8) & 0x03) as u8);
        out.push((left & 0xFF) as u8);
        out.extend_from_slice(&word.to_be_bytes());
        out
    }

    /// One RLE run, in nibbles, as the format packs it: the low two bits are
    /// the colour and the rest is the run length.
    pub fn run(entry: u8, length: u16) -> Vec<u8> {
        let code = (u32::from(length) << 2) | u32::from(entry & 3);
        match length {
            0 => vec![entry & 3],
            1..=3 => vec![code as u8],
            4..=15 => vec![(code >> 4) as u8, (code & 0x0F) as u8],
            16..=63 => vec![
                (code >> 8) as u8,
                ((code >> 4) & 0x0F) as u8,
                (code & 0x0F) as u8,
            ],
            _ => vec![
                (code >> 12) as u8,
                ((code >> 8) & 0x0F) as u8,
                ((code >> 4) & 0x0F) as u8,
                (code & 0x0F) as u8,
            ],
        }
    }

    /// Pack a run of nibbles into bytes, padding the tail.
    pub fn nibbles(values: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(values.len().div_ceil(2));
        for pair in values.chunks(2) {
            let high = pair[0] << 4;
            let low = pair.get(1).copied().unwrap_or(0);
            out.push(high | low);
        }
        out
    }

    /// One field's worth of lines, each a list of `(entry, length)` runs. A run
    /// of length 0 fills to the end of the line.
    pub fn field(lines: &[&[(u8, u16)]]) -> Vec<u8> {
        let mut values = Vec::new();
        for line in lines {
            for (entry, length) in line.iter() {
                values.extend_from_slice(&run(*entry, *length));
            }
            // Byte alignment between lines, which the decoder assumes.
            if values.len() % 2 == 1 {
                values.push(0);
            }
        }
        nibbles(&values)
    }

    /// Assemble a subpicture unit: two fields of pixel data, then the control
    /// blocks, with every offset filled in.
    pub fn packet(top_field: &[u8], bottom_field: &[u8], blocks: Vec<Block>) -> Vec<u8> {
        let mut out = vec![0u8; PACKET_HEADER];
        let top_at = out.len();
        out.extend_from_slice(top_field);
        let bottom_at = out.len();
        out.extend_from_slice(bottom_field);
        let control_at = out.len();

        // Each block: delay, next offset, commands, END.
        let mut bodies = Vec::new();
        let mut at = control_at;
        for block in &blocks {
            let mut body = Vec::new();
            body.extend_from_slice(&block.delay.to_be_bytes());
            body.extend_from_slice(&[0, 0]); // patched below
            body.extend_from_slice(&block.commands);
            body.push(CMD_END);
            bodies.push((at, body.len()));
            at += body.len();
        }
        for (index, block) in blocks.iter().enumerate() {
            let (start, len) = bodies[index];
            let next = if index + 1 < bodies.len() {
                bodies[index + 1].0
            } else {
                start // the last block points at itself
            };
            let mut body = Vec::new();
            body.extend_from_slice(&block.delay.to_be_bytes());
            body.extend_from_slice(&(next as u16).to_be_bytes());
            body.extend_from_slice(&block.commands);
            body.push(CMD_END);
            debug_assert_eq!(body.len(), len);
            debug_assert_eq!(out.len(), start);
            out.extend_from_slice(&body);
        }

        let size = out.len() as u16;
        out[0..2].copy_from_slice(&size.to_be_bytes());
        out[2..4].copy_from_slice(&(control_at as u16).to_be_bytes());
        let _ = (top_at, bottom_at, BLOCK_HEADER);
        out
    }

    /// THE MINIMAL UNIT: a 4x2 picture at (100, 200) on the DVD grid, shown at
    /// the packet's own time and taken away half a second later. The two fields
    /// differ by construction: the top field's rows are palette entry 1 and
    /// the bottom field's entry 2, so a decoder that reads one field twice, or
    /// reads them in the wrong order, cannot pass.
    pub fn minimal_unit() -> Vec<u8> {
        let top = field(&[&[(1, 4)]]);
        let bottom = field(&[&[(2, 4)]]);
        let fields_at = (PACKET_HEADER as u16, (PACKET_HEADER + top.len()) as u16);
        packet(
            &top,
            &bottom,
            vec![
                Block {
                    delay: 0,
                    commands: [
                        set_color([0, 1, 2, 3]),
                        set_alpha([0, 15, 15, 15]),
                        set_area(100, 200, 103, 201),
                        set_fields(fields_at.0, fields_at.1),
                        display_on(),
                    ]
                    .concat(),
                },
                Block {
                    delay: 45,
                    commands: display_off(),
                },
            ],
        )
    }

    /// A unit whose schedule never stops: the container's duration is what ends
    /// it.
    pub fn unit_without_a_stop() -> Vec<u8> {
        let top = field(&[&[(1, 4)]]);
        let bottom = field(&[&[(1, 4)]]);
        let fields_at = (PACKET_HEADER as u16, (PACKET_HEADER + top.len()) as u16);
        packet(
            &top,
            &bottom,
            vec![Block {
                delay: 0,
                commands: [
                    set_color([0, 1, 2, 3]),
                    set_alpha([0, 15, 15, 15]),
                    set_area(0, 0, 3, 1),
                    set_fields(fields_at.0, fields_at.1),
                    display_on(),
                ]
                .concat(),
            }],
        )
    }

    /// The two-byte delivery that closes an open-ended subtitle.
    pub fn terminator() -> Vec<u8> {
        vec![0, 0]
    }

    /// Every vector above, named, for a fuzz seed corpus.
    pub fn seed_corpus() -> Vec<(&'static str, Vec<u8>)> {
        vec![
            ("minimal", minimal_unit()),
            ("no-stop", unit_without_a_stop()),
            ("terminator", terminator()),
        ]
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::{fixtures::*, *};
    use crate::{cue::CueEngine, subpic::BitmapFormat, video::OverlaySpace};

    fn packet_at(bytes: &[u8], rt_ms: u64, duration_ms: Option<u64>) -> BitmapPacket {
        BitmapPacket {
            format: BitmapFormat::Vobsub,
            data: gst::Buffer::from_slice(bytes.to_vec()),
            codec_data: None,
            rt: gst::ClockTime::from_mseconds(rt_ms),
            duration: duration_ms.map(gst::ClockTime::from_mseconds),
        }
    }

    fn taught() -> VobsubDecoder {
        let mut decoder = VobsubDecoder::new();
        decoder.set_codec_data(SAMPLE_IDX);
        decoder.set_video_size(720, 480);
        decoder
    }

    fn pixel(region: &BitmapRegion, x: u32, y: u32) -> Rgba {
        let at = ((y * region.width + x) * 4) as usize;
        region.pixels[at..at + 4].try_into().expect("four bytes")
    }

    fn wait_for(condition: impl Fn() -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline {
            if condition() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        condition()
    }

    /// THE REAL PALETTE, from the real sample's CodecPrivate.
    ///
    /// And the finding that made this test worth writing: the `.idx` palette is
    /// RGB, not the YCbCr the DVD's own CLUT carries and the reference
    /// converts. The sample's entries settle it: `101010`, `a1a1a1`,
    /// `c5c5c5`, `ebebeb` are a grey ramp read as RGB and four unrelated
    /// saturated colours read as YCbCr.
    #[test]
    fn the_containers_idx_palette_is_rgb() {
        gst::init().unwrap();
        let mut decoder = VobsubDecoder::new();
        decoder.set_codec_data(SAMPLE_IDX);

        assert_eq!(decoder.grid, Some((720, 480)), "the size line");
        let clut = decoder.clut.expect("a palette");
        assert_eq!(clut[0], [0x0d, 0x00, 0xee, 0xff], "entry 0, a blue");
        assert_eq!(clut[2], [0x10, 0x10, 0x10, 0xff], "entry 2, near black");
        assert_eq!(clut[3], [0xeb, 0xeb, 0xeb, 0xff], "entry 3, near white");
        // The grey ramp is the whole argument: read as YCbCr these four would
        // be a blue, a pink and two greens.
        for entry in [2usize, 8, 9, 3] {
            let [r, g, b, _] = clut[entry];
            assert!(
                r == g && g == b,
                "entry {entry} is not grey ({r},{g},{b}), so the palette is not being read as RGB"
            );
        }
    }

    /// One unit in, one picture out, with its schedule.
    #[test]
    fn a_minimal_unit_becomes_one_region_with_straight_alpha() {
        gst::init().unwrap();
        let mut decoder = taught();

        let updates = decoder.push(&packet_at(&minimal_unit(), 1_000, None));
        assert_eq!(updates.len(), 1, "one picture");
        let update = &updates[0];
        assert_eq!(update.start_rt, gst::ClockTime::from_mseconds(1_000));
        // 45 units of 1024/90000 s is 512 ms.
        assert_eq!(update.end_rt, Some(gst::ClockTime::from_mseconds(1_512)));
        assert_eq!(update.regions.len(), 1);

        let region = &update.regions[0];
        assert_eq!((region.width, region.height), (4, 2), "the display area");
        assert_eq!(
            (region.x, region.y),
            (100, 200),
            "grid and video agree, so the rect is the area's own"
        );
        assert_eq!(decoder.take_decode_errors(), 0);
    }

    /// The straight-alpha canary, and the two fields.
    ///
    /// The two rows come from DIFFERENT fields by construction: the top
    /// field's line is one colour and the bottom field's another, so a decoder
    /// that reads one field twice, or swaps them, cannot pass. The alpha is
    /// 8/15, which widens to 0x88: with the reference's premultiply
    /// (`gstspu-vobsub-render.c:62-64`) the colour would come out scaled by
    /// 136/255 and this renderer would wash it out.
    #[test]
    fn the_two_fields_interleave_and_the_alpha_stays_straight() {
        gst::init().unwrap();
        let mut decoder = taught();

        // TWO LINES PER FIELD, and the second line of each is what makes this
        // test bite. The fields are adjacent in the packet, so a decoder that
        // ignored the interleave and read straight through would fall out of
        // the top field into the bottom one and produce the right two rows by
        // accident; with a spare line in each, reading straight through gives
        // the top field twice.
        let top = field(&[&[(1, 4)], &[(1, 4)]]);
        let bottom = field(&[&[(2, 4)], &[(2, 4)]]);
        let at = (PACKET_HEADER as u16, (PACKET_HEADER + top.len()) as u16);
        let bytes = packet(
            &top,
            &bottom,
            vec![Block {
                delay: 0,
                commands: [
                    set_color([0, 1, 2, 3]),
                    // entry 1 half-transparent, entry 2 opaque
                    set_alpha([0, 8, 15, 15]),
                    set_area(0, 0, 3, 1),
                    set_fields(at.0, at.1),
                    display_on(),
                ]
                .concat(),
            }],
        );

        let updates = decoder.push(&packet_at(&bytes, 0, None));
        let region = &updates[0].regions[0];
        assert_eq!(
            pixel(region, 0, 0),
            [0xee, 0x45, 0x0d, 0x88],
            "THE CANARY: the top field's colour came out premultiplied"
        );
        assert_eq!(
            pixel(region, 0, 1),
            [0x10, 0x10, 0x10, 0xff],
            "the second row must come from the OTHER field"
        );
        assert_ne!(
            pixel(region, 0, 0),
            pixel(region, 0, 1),
            "both rows decoded from the same field"
        );
    }

    /// A two-byte delivery is a terminator, not a subpicture: dropped, and not
    /// counted against the stream.
    #[test]
    fn a_terminator_is_dropped_without_a_complaint() {
        gst::init().unwrap();
        let mut decoder = taught();

        assert!(
            decoder
                .push(&packet_at(&terminator(), 5_000, None))
                .is_empty()
        );
        assert_eq!(
            decoder.take_decode_errors(),
            0,
            "a terminator is not an error"
        );
        // And the decoder still works: this format cannot cascade.
        assert_eq!(
            decoder.push(&packet_at(&minimal_unit(), 6_000, None)).len(),
            1
        );
    }

    /// CHG_COLCON: a band of scanlines drawn with a different palette from a
    /// given x, which is how this format does karaoke and highlights.
    #[test]
    fn a_colour_change_repaints_part_of_a_line() {
        gst::init().unwrap();
        let mut decoder = taught();

        let top = field(&[&[(1, 4)]]);
        let bottom = field(&[&[(1, 4)]]);
        let at = (PACKET_HEADER as u16, (PACKET_HEADER + top.len()) as u16);
        // From x=2 onward, entry 1 becomes clut index 3 (near white).
        let change = change_colour(&[change_entry(0, 1, 2, [0, 3, 2, 3], [0, 15, 15, 15])]);
        let bytes = packet(
            &top,
            &bottom,
            vec![Block {
                delay: 0,
                commands: [
                    set_color([0, 1, 2, 3]),
                    set_alpha([0, 15, 15, 15]),
                    set_area(0, 0, 3, 1),
                    set_fields(at.0, at.1),
                    change,
                    display_on(),
                ]
                .concat(),
            }],
        );

        let updates = decoder.push(&packet_at(&bytes, 0, None));
        let region = &updates[0].regions[0];
        assert_eq!(
            pixel(region, 0, 0),
            [0xee, 0x45, 0x0d, 0xff],
            "before the change, the main palette"
        );
        assert_eq!(
            pixel(region, 2, 0),
            [0xeb, 0xeb, 0xeb, 0xff],
            "from the change's x, the override's palette"
        );
        assert_eq!(decoder.take_decode_errors(), 0);
    }

    /// The authoring grid is STRETCHED onto the video, both axes independently.
    ///
    /// Not fitted: a DVD frame's pixels are not square, and preserving the
    /// authoring aspect would misplace a subtitle on exactly the anamorphic
    /// content the format was made for.
    #[test]
    fn the_authoring_grid_is_stretched_onto_the_video() {
        gst::init().unwrap();

        let place = |video: (u32, u32)| {
            let mut decoder = VobsubDecoder::new();
            decoder.set_codec_data(SAMPLE_IDX);
            decoder.set_video_size(video.0, video.1);
            let updates = decoder.push(&packet_at(&minimal_unit(), 0, None));
            let region = &updates[0].regions[0];
            (
                region.x,
                region.y,
                region.render_width,
                region.render_height,
            )
        };

        assert_eq!(place((720, 480)), (100, 200, 4, 2), "the grid's own size");
        assert_eq!(place((1440, 960)), (200, 400, 8, 4), "twice, both axes");
        // 1920x1080 against 720x480 is 2.667 across and 2.25 down: the two
        // scales differ, which is what makes this a stretch.
        assert_eq!(place((1920, 1080)), (267, 450, 11, 5));
    }

    /// A schedule that runs BACKWARDS still comes out forwards.
    ///
    /// Nothing in the format stops a control chain from pointing at a block
    /// whose delay is smaller than the one before it, and the fuzz target found
    /// what that did: an update that ended a minute before it started, which
    /// nothing downstream can schedule. A block that is already due when the
    /// chain reaches it happens now.
    #[test]
    fn a_schedule_that_runs_backwards_still_comes_out_forwards() {
        gst::init().unwrap();
        let mut decoder = taught();

        let top = field(&[&[(1, 4)]]);
        let at = (PACKET_HEADER as u16, (PACKET_HEADER + top.len()) as u16);
        let bytes = packet(
            &top,
            &field(&[&[(1, 4)]]),
            vec![
                Block {
                    delay: 900,
                    commands: [
                        set_color([0, 1, 2, 3]),
                        set_alpha([0, 15, 15, 15]),
                        set_area(0, 0, 3, 1),
                        set_fields(at.0, at.1),
                        display_on(),
                    ]
                    .concat(),
                },
                // Earlier than the block that showed the picture.
                Block {
                    delay: 10,
                    commands: display_off(),
                },
            ],
        );

        let updates = decoder.push(&packet_at(&bytes, 1_000, None));
        assert_eq!(updates.len(), 1);
        let update = &updates[0];
        assert!(
            update.end_rt.is_some_and(|end| end >= update.start_rt),
            "a picture that ends before it starts: {:?}..{:?}",
            update.start_rt,
            update.end_rt
        );
    }

    /// A schedule with no stop in it ends when the CONTAINER says, and stays
    /// open-ended when even that is missing.
    #[test]
    fn a_schedule_without_a_stop_falls_back_to_the_buffer_duration() {
        gst::init().unwrap();
        let mut decoder = taught();

        let updates = decoder.push(&packet_at(&unit_without_a_stop(), 1_000, Some(800)));
        assert_eq!(
            updates[0].end_rt,
            Some(gst::ClockTime::from_mseconds(1_800)),
            "matroska's BlockDuration is the fallback end"
        );

        let updates = decoder.push(&packet_at(&unit_without_a_stop(), 3_000, None));
        assert_eq!(
            updates[0].end_rt, None,
            "with no duration either, the picture is open-ended"
        );
    }

    /// Malformed packets are counted resets and never panics, and, because a
    /// subpicture unit is self-contained, they cannot cascade into the next
    /// one.
    #[test]
    fn malformed_packets_are_counted_resets_and_never_panics() {
        gst::init().unwrap();

        // A control offset past the end of the packet.
        let mut decoder = taught();
        assert!(
            decoder
                .push(&packet_at(&[0, 8, 0xFF, 0xFF, 0, 0, 0, 0], 0, None))
                .is_empty()
        );
        assert_eq!(decoder.take_decode_errors(), 1);

        // A packet claiming more bytes than it carries.
        let mut decoder = taught();
        assert!(
            decoder
                .push(&packet_at(&[0xFF, 0xFF, 0, 4, 0, 0], 0, None))
                .is_empty()
        );
        assert_eq!(decoder.take_decode_errors(), 1);

        // A packet declaring LESS than its own header, which the fuzz target
        // found: the buffer is long enough and the packet says it is three
        // bytes, so re-slicing to what it claims leaves no header to read.
        let mut decoder = taught();
        assert!(
            decoder
                .push(&packet_at(&[0, 3, 0, 4, 0, 0, 0, 0], 0, None))
                .is_empty()
        );
        assert_eq!(decoder.take_decode_errors(), 1);

        // Noise, at length.
        let mut decoder = taught();
        let mut noise = Vec::new();
        for index in 0..4096u32 {
            noise.push((index.wrapping_mul(2_654_435_761) >> 13) as u8);
        }
        decoder.push(&packet_at(&noise, 0, None));
        assert_eq!(
            decoder.push(&packet_at(&minimal_unit(), 100, None)).len(),
            1,
            "a self-contained format must not carry a failure into the next packet"
        );
    }

    /// THE CAP IS ON THE PACKET'S PRODUCTION, not only on what survives it.
    ///
    /// Found by the exit gate's hour-long campaign, and it is the defect a
    /// retention-only cap cannot see: a VOBSUB packet is self-contained, so
    /// this decoder retains almost nothing and `held_bytes()` stayed tiny,
    /// while ONE packet's command schedule showed, stopped and re-showed the
    /// same 11 MB picture nearly two hundred times, and every one of those is a
    /// separate allocation in the vector handed back by a single `push`. The
    /// fuzzer measured 2.19 GB live in 194 allocations, all from `render`.
    ///
    /// A stream cannot be allowed to make the receiver take gigabytes at once,
    /// whether or not the decoder lets go of them afterwards.
    #[test]
    fn a_packet_producing_more_than_the_budget_is_a_counted_reset() {
        gst::init().unwrap();
        let mut decoder = taught();

        // 1024x1024 RGBA is 4 MiB a picture: nine of them is past the cap and
        // none of them is, which is the distinction being made.
        let top = field(&[&[(1, 8)]]);
        let at = (PACKET_HEADER as u16, PACKET_HEADER as u16);
        let show = [
            set_color([0, 1, 2, 3]),
            set_alpha([0, 15, 15, 15]),
            set_area(0, 0, 1023, 1023),
            set_fields(at.0, at.1),
            display_on(),
        ]
        .concat();
        let mut blocks = Vec::new();
        for index in 0..9u16 {
            blocks.push(Block {
                delay: index * 2,
                commands: show.clone(),
            });
            blocks.push(Block {
                delay: index * 2 + 1,
                commands: display_off(),
            });
        }
        let bytes = packet(&top, &[], blocks);

        assert!(
            decoder.push(&packet_at(&bytes, 0, None)).is_empty(),
            "a packet produced 36 MiB of pictures inside a 32 MiB budget"
        );
        assert_eq!(decoder.budget_resets(), 1);
        assert_eq!(decoder.take_decode_errors(), 1);

        // And a packet that shows the same picture a FEW times is still fine:
        // what is refused is the total, not the repetition.
        let mut blocks = Vec::new();
        for index in 0..3u16 {
            blocks.push(Block {
                delay: index * 2,
                commands: show.clone(),
            });
            blocks.push(Block {
                delay: index * 2 + 1,
                commands: display_off(),
            });
        }
        let bytes = packet(&top, &[], blocks);
        let updates = decoder.push(&packet_at(&bytes, 1_000, None));
        assert_eq!(updates.len(), 3, "three 4 MiB pictures fit a 32 MiB budget");
        assert_eq!(decoder.take_decode_errors(), 0);
    }

    /// The allocation cap, at the one place this decoder allocates: a display
    /// area is twelve bits per axis, so a packet can ask for 4095x4095 RGBA
    /// (64 MiB) out of a few dozen bytes.
    #[test]
    fn a_picture_past_the_budget_is_a_counted_reset() {
        gst::init().unwrap();
        let mut decoder = taught();

        let top = field(&[&[(1, 4)]]);
        let at = (PACKET_HEADER as u16, PACKET_HEADER as u16);
        let bytes = packet(
            &top,
            &[],
            vec![Block {
                delay: 0,
                commands: [
                    set_color([0, 1, 2, 3]),
                    set_alpha([0, 15, 15, 15]),
                    set_area(0, 0, 4095, 4095),
                    set_fields(at.0, at.1),
                    display_on(),
                ]
                .concat(),
            }],
        );

        assert!(
            decoder.push(&packet_at(&bytes, 0, None)).is_empty(),
            "a 64 MiB picture was built inside a 32 MiB budget"
        );
        assert_eq!(decoder.budget_resets(), 1);
        assert_eq!(decoder.take_decode_errors(), 1);
        assert!(decoder.held_bytes() >= decoder.allocated_bytes());

        // Recovery: the cap resets the decoder, it does not disable it.
        assert_eq!(
            decoder.push(&packet_at(&minimal_unit(), 100, None)).len(),
            1
        );
    }

    /// With no `.idx` at all (a container that supplied no CodecPrivate) the
    /// picture is still drawn, in the reference's guessed ramp of white and
    /// grey. Unreadable colours beat no subtitles.
    #[test]
    fn a_stream_with_no_palette_still_draws() {
        gst::init().unwrap();
        let mut decoder = VobsubDecoder::new();
        decoder.set_video_size(720, 480);

        let updates = decoder.push(&packet_at(&minimal_unit(), 0, None));
        let region = &updates[0].regions[0];
        assert_eq!(
            pixel(region, 0, 0),
            [255, 255, 255, 0xff],
            "the first visible entry is white"
        );
        assert_eq!(
            pixel(region, 0, 1),
            [127, 127, 127, 0xff],
            "the next is grey"
        );
    }

    /// B10's ENGINE HALF, and the payoff of a self-contained format: a unit
    /// covering the FROZEN frame reaches the screen with no frame flowing and
    /// nothing delivered before it.
    ///
    /// This is the B7 staging with a real decoder under it, and the assertion
    /// that matters is the second one: the engine is handed ONE packet, out of
    /// nowhere, at a position it is already stopped at: no epoch, no palette
    /// from an earlier delivery, no half-built object store, and a picture
    /// appears. PGS cannot be driven this way (its display set is a delta on an
    /// epoch it needs to have seen); VOBSUB can, which is the point of the
    /// paused path.
    #[test]
    fn a_paused_unit_covering_the_frozen_frame_reaches_the_screen() {
        gst::init().unwrap();
        let engine = CueEngine::new();
        engine.set_video_size(720, 480);

        // The frame the sink is stopped on.
        let frozen = gst::ClockTime::from_mseconds(4_000);
        engine.overlays_for(Some(frozen));
        assert!(
            engine.current_overlays().is_empty(),
            "nothing on screen yet"
        );

        let changes = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let counter = changes.clone();
        engine.set_on_change(move || {
            counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        });

        // ONE packet, covering the frozen frame, with its palette on the side.
        let mut packet = packet_at(&minimal_unit(), frozen.mseconds(), None);
        packet.codec_data = Some(gst::Buffer::from_slice(SAMPLE_IDX.to_vec()));
        engine.submit_bitmap(packet);

        assert!(
            wait_for(|| changes.load(std::sync::atomic::Ordering::Relaxed) > 0),
            "the renderer was never told to repaint, so a paused frame would never show it"
        );
        assert!(
            !engine.current_overlays().is_empty(),
            "a self-contained unit covering the frozen frame did not reach the screen"
        );
        assert_eq!(engine.bitmap_decode_errors(), 0);
    }

    /// THE RENDER PROOF: a real subpicture unit through the ENGINE's production
    /// wiring (no test decoder installed, so the decoder under this is the one
    /// `subpic::decoder_for` builds) and out as a source-frame overlay.
    #[test]
    fn a_unit_reaches_the_overlay_set_through_the_production_decoder() {
        gst::init().unwrap();
        let engine = CueEngine::new();
        engine.set_video_size(720, 480);

        let mut packet = packet_at(&minimal_unit(), 1_000, None);
        packet.codec_data = Some(gst::Buffer::from_slice(SAMPLE_IDX.to_vec()));
        engine.submit_bitmap(packet);

        let at = gst::ClockTime::from_mseconds(1_200);
        assert!(
            wait_for(|| !engine.overlays_for(Some(at)).is_empty()),
            "the decoded unit never reached the overlay set"
        );
        let overlays = engine.overlays_for(Some(at));
        assert_eq!(overlays.len(), 1);
        assert_eq!(overlays[0].space, OverlaySpace::SrcFrame);
        assert_eq!((overlays[0].x, overlays[0].y), (100, 200));
        assert_eq!(
            &overlays[0].pixels[0..4],
            &[0xee, 0x45, 0x0d, 0xff],
            "these are the decoder's own pixels, from the container's palette"
        );
        assert_eq!(engine.bitmap_decode_errors(), 0);
        assert_eq!(engine.bitmap_overflow_resets(), 0);
        assert_eq!(engine.bitmap_dropped_sets(), 0);

        // And the schedule takes it away on its own, with no further packet.
        let after = gst::ClockTime::from_mseconds(1_600);
        assert!(
            engine.overlays_for(Some(after)).is_empty(),
            "the unit's own stop time never took the picture off the screen"
        );
    }
}
