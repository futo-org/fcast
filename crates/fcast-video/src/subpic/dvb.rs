//! DVB subtitles, ETSI EN 300 743 (`subpicture/x-dvb`).
//!
//! The broadcast format, and the expensive one. Where PGS delivers a whole
//! picture per display set and VOBSUB a whole subtitle per packet, DVB paints
//! into PERSISTENT REGION BUFFERS that survive from one display set to the
//! next: a page can put four regions on screen, then send a display set that
//! repaints twelve pixels of one of them and re-shows all four. Nothing else
//! in this module keeps state that a stream can grow, which is why the
//! allocation accounting the earlier verticals built exists at all.
//!
//! # Structure, as this decoder reads it
//!
//! The transport hands over a PES data field:
//!
//! ```text
//! [0x20 data identifier][0x00 subtitle stream id]
//!   ( [0x0f sync][type u8][page id u16be][length u16be][payload] )*
//! ```
//!
//! and six segment types matter:
//!
//! | type | segment | what it does |
//! |---|---|---|
//! | 0x10 | page composition | the page's timeout, its state, and which regions are on it |
//! | 0x11 | region composition | a region's size, depth, palette and background, and which objects paint into it |
//! | 0x12 | CLUT definition | palette entries, in YCbCr with transparency |
//! | 0x13 | object data | the run-length-encoded pixels, in two interlaced fields |
//! | 0x14 | display definition | the coordinate grid, and optionally a window inside it |
//! | 0x80 | end of display set | everything above becomes a picture |
//!
//! A DISPLAY SET is every packet sharing one running time. The 0x80 segment
//! closes one eagerly; a packet arriving at a NEW running time force-terminates
//! whatever is open, because the segments that follow describe a different
//! moment. (The reference does the same, from the same reasoning about PTS.)
//!
//! # What survives what
//!
//! Region buffers, CLUTs and the page's composition all persist across display
//! sets. Exactly one thing empties them: a page composition segment whose
//! `page_state` is 2, a MODE CHANGE. An acquisition point (state 1) does not,
//! and neither does a normal page (state 0), which is the whole reason a
//! twelve-pixel update can be a legal display set.
//!
//! # Timing
//!
//! `start_rt` is the display set's own running time. `end_rt` is that plus the
//! page's `page_time_out`, in SECONDS, with zero clamped to one. A page that
//! says it lasts no time at all is a page that has not been authored, and one
//! second is the shortest thing worth showing. A later display set supersedes
//! it before then, which is the normal case; the timeout is what takes a
//! subtitle away when the stream simply stops talking about it.
//!
//! # Geometry
//!
//! The display definition segment names the coordinate grid (default 720x576)
//! and may name a window inside it; region positions are relative to that
//! window. The mapping onto the video is a STRETCH, as it is for VOBSUB and
//! unlike PGS: a broadcast grid is a 4:3 or 16:9 raster whose pixels are not
//! square, so fitting it would misplace subtitles on anamorphic content. PGS is
//! the odd one out because its canvas is authored in square pixels against a
//! known video size, and fitting is what keeps a 1920x1080-authored subtitle on
//! the picture when the video is 1280x720.
//!
//! # Provenance
//!
//! Written fresh against ETSI EN 300 743, with GStreamer's `dvb-sub.c` (LGPL,
//! itself ported from ffmpeg's `dvbsubdec.c`) consulted as a NORMATIVE
//! REFERENCE for what the spec leaves open or states loosely. Each consultation
//! is named at the site that made the decision: the three run-length codings
//! bit for bit, the map-table pseudo-codes and their defaults, the
//! `non_modifying_colour` rule, which field a region's first line belongs to,
//! the `end of object line` code advancing two lines, the default CLUT's exact
//! colours, the "8k region / 16k display" sanity clamps, refusing an object
//! segment for an object no region has claimed, and the depth-vs-string-type
//! refusals. Nothing is transliterated: the reference's cross-linked object and
//! display lists are replaced by placements owned by the region they paint
//! into, the error policy is this crate's counted-reset discipline, the
//! accounting is new, and the output is straight-alpha RGBA regions for a
//! compositor rather than AYUV planes for a blender.

use std::{collections::HashMap, sync::Arc};

use tracing::{debug, warn};

use super::{
    BitmapPacket, BitmapRegion, DisplayUpdate, SubpicDecoder,
    pgs::{ALLOCATION_BUDGET, Rgba},
};

/// The PES data field's two identifying bytes.
const DATA_IDENTIFIER: u8 = 0x20;
const SUBTITLE_STREAM_ID: u8 = 0x00;
/// Every segment starts with this.
const SYNC_BYTE: u8 = 0x0F;

const SEGMENT_PAGE: u8 = 0x10;
const SEGMENT_REGION: u8 = 0x11;
const SEGMENT_CLUT: u8 = 0x12;
const SEGMENT_OBJECT: u8 = 0x13;
const SEGMENT_DISPLAY_DEFINITION: u8 = 0x14;
const SEGMENT_END_OF_DISPLAY_SET: u8 = 0x80;
const SEGMENT_STUFFING: u8 = 0xFF;

/// `[sync][type][page id u16][length u16]`.
const SEGMENT_HEADER: usize = 6;

/// What one object placement and one page entry cost, for the accounting.
/// Named rather than inlined so the two functions that charge them cannot
/// drift apart.
const PLACEMENT_BYTES: usize = std::mem::size_of::<Placement>();
const PAGE_REGION_BYTES: usize = std::mem::size_of::<PageRegion>();

/// The grid a stream is assumed to use until a display definition segment says
/// otherwise: standard-definition broadcast.
const DEFAULT_DISPLAY: (u32, u32) = (720, 576);

/// The reference's sanity clamps, kept: 8k for a region, 16k for the display.
/// Both are far past anything a broadcast uses and far below what the 16-bit
/// fields allow, which is the point: the fields allow 65535x65535, and one
/// such region is 4 GiB of indices.
const MAX_REGION_SIDE: u32 = 8192;
const MAX_DISPLAY_SIDE: u32 = 16384;

/// How many regions and CLUTs one stream can keep alive. Both are keyed by a
/// byte, so these are the format's own ceilings rather than a policy, and
/// because they are, they need no runtime check: a map keyed by `u8` cannot
/// hold a two hundred and fifty-seventh entry, so a guard against one would be
/// unreachable code pretending to be a bound. They
/// are still what [`TABLE_RESERVE`] is computed from.
const MAX_REGIONS: usize = 256;
const MAX_CLUTS: usize = 256;

/// Room kept aside for the two hash tables' own storage.
///
/// RESERVED rather than checked, for the same reason PGS reserves its carry: a
/// table grows when an entry is inserted, which is AFTER the price has been
/// agreed, so a check that reads the table's current size cannot see the growth
/// coming. Two hundred and fifty-six of each is the format's ceiling and the
/// doubling is the allocator's, so twice the entry cost covers both.
const TABLE_RESERVE: u64 = 2
    * (MAX_REGIONS * (std::mem::size_of::<(u8, Region)>() + 1)
        + MAX_CLUTS * (std::mem::size_of::<(u8, Clut)>() + 1)) as u64;

/// What everything except those tables may hold, so that
/// `held_bytes() <= ALLOCATION_BUDGET` is exact.
const WORKING_BUDGET: u64 = ALLOCATION_BUDGET - TABLE_RESERVE;

/// Why a display set is being thrown away. The third decoder to carry this
/// enum, and deliberately identical to the other two: one error policy, with
/// the budget's own reason kept separate so a test can tell them apart.
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

/// A palette, at all three depths the format defines.
///
/// Entries are straight-alpha RGBA, converted once when the CLUT is defined.
/// Kept inline (no heap) so that what this decoder is HOLDING is exactly the
/// region buffers plus a fixed cost per palette, which is what makes the
/// accounting exact.
#[derive(Debug, Clone, Copy)]
struct Clut {
    two_bit: [Rgba; 4],
    four_bit: [Rgba; 16],
    eight_bit: [Rgba; 256],
}

impl Clut {
    /// The spec's default CLUT, which every palette starts as and an undefined
    /// one stays.
    ///
    /// The colours are the reference's, entry for entry: transparent, white,
    /// black and grey at two bits; the eight primaries at full and half
    /// intensity at four; and at eight bits a 6-bit colour cube crossed with
    /// four transparency classes selected by bits 3 and 7. They are taken
    /// rather than re-derived because the spec's table and twenty years of
    /// receivers agreeing on it are two different things, and the second is
    /// what a viewer sees.
    ///
    /// Built straight in RGBA. The reference converts each colour to AYUV
    /// because its compositor blends in AYUV; going RGB → YCbCr → RGB here
    /// would only lose the low bits of every default colour.
    fn default_clut() -> Self {
        let mut clut = Self {
            two_bit: [[0; 4]; 4],
            four_bit: [[0; 4]; 16],
            eight_bit: [[0; 4]; 256],
        };

        clut.two_bit[0] = [0, 0, 0, 0];
        clut.two_bit[1] = [255, 255, 255, 255];
        clut.two_bit[2] = [0, 0, 0, 255];
        clut.two_bit[3] = [127, 127, 127, 255];

        clut.four_bit[0] = [0, 0, 0, 0];
        for index in 1..16usize {
            let level = if index < 8 { 255 } else { 127 };
            clut.four_bit[index] = [
                if index & 1 != 0 { level } else { 0 },
                if index & 2 != 0 { level } else { 0 },
                if index & 4 != 0 { level } else { 0 },
                255,
            ];
        }

        clut.eight_bit[0] = [0, 0, 0, 0];
        for index in 1..256usize {
            let entry = if index < 8 {
                [
                    if index & 1 != 0 { 255 } else { 0 },
                    if index & 2 != 0 { 255 } else { 0 },
                    if index & 4 != 0 { 255 } else { 0 },
                    63,
                ]
            } else {
                let low = |bit: usize, small: u8, large: u8| {
                    (if index & bit != 0 { small } else { 0 })
                        + (if index & (bit << 4) != 0 { large } else { 0 })
                };
                match index & 0x88 {
                    0x00 => [low(1, 85, 170), low(2, 85, 170), low(4, 85, 170), 255],
                    0x08 => [low(1, 85, 170), low(2, 85, 170), low(4, 85, 170), 127],
                    0x80 => [
                        127 + low(1, 43, 85),
                        127 + low(2, 43, 85),
                        127 + low(4, 43, 85),
                        255,
                    ],
                    _ => [low(1, 43, 85), low(2, 43, 85), low(4, 43, 85), 255],
                }
            };
            clut.eight_bit[index] = entry;
        }

        clut
    }

    fn entry(&self, depth: u8, index: u8) -> Rgba {
        match depth {
            2 => self.two_bit[usize::from(index) & 3],
            8 => self.eight_bit[usize::from(index)],
            _ => self.four_bit[usize::from(index) & 15],
        }
    }
}

/// Where one object paints inside a region.
#[derive(Debug, Clone, Copy)]
struct Placement {
    object_id: u16,
    x: u32,
    y: u32,
}

/// A region: a persistent buffer of CLUT INDICES, one byte per pixel, plus what
/// it takes to turn those into colours.
#[derive(Debug)]
struct Region {
    width: u32,
    height: u32,
    depth: u8,
    clut_id: u8,
    background: u8,
    /// One byte per pixel: the CLUT index, not the colour. This is the buffer
    /// that survives display sets and the one the budget is really about.
    pixels: Vec<u8>,
    placements: Vec<Placement>,
    /// The RGBA this region last expanded into, kept until something changes
    /// it.
    ///
    /// A DVB page is re-shown constantly (a display set that repaints twelve
    /// pixels of one region re-emits all four) and expanding a region costs a
    /// pass over every pixel. Keeping the last expansion makes a re-show nearly
    /// free (0.72 ms to near zero on a full page, measured by a
    /// review 3), and it makes something else true that the engine already
    /// believed: the pending store's byte budget accounts by ALLOCATION and
    /// dedupes shared pixels by pointer, on the stated grounds that "DVB
    /// re-emits the page whenever any part of it changes, so N updates of one
    /// page are N pointers to ONE allocation". Without this cache that
    /// sentence was false (every set allocated a fresh `Arc`) and so was the
    /// engine's A7 no-op adoption check, which compares by pointer and so never
    /// fired for a redelivered DVB page.
    ///
    /// Invalidated by anything that changes what the region looks like: a
    /// paint, a composition, or a palette definition (see
    /// [`DvbDecoder::clut_segment`], which cannot know which regions use the
    /// palette it just changed).
    expanded: Option<Arc<Vec<u8>>>,
}

impl Region {
    /// What this region is holding: the indices, the placements, and the
    /// expansion if it has one.
    fn allocated(&self) -> u64 {
        self.pixels.capacity() as u64
            + (self.placements.capacity() * PLACEMENT_BYTES) as u64
            + self.expanded.as_ref().map_or(0, |rgba| rgba.len() as u64)
    }

    /// The same, as a CHARGE: the geometry the stream declared, whether or not
    /// the buffer has been filled yet.
    fn charge(&self) -> u64 {
        let declared = u64::from(self.width) * u64::from(self.height);
        declared.max(self.pixels.capacity() as u64)
            + (self.placements.capacity() * PLACEMENT_BYTES) as u64
            + self.expanded.as_ref().map_or(0, |rgba| rgba.len() as u64)
    }

    /// Throw the expansion away. Cheap to rebuild, and wrong to keep.
    fn invalidate(&mut self) {
        self.expanded = None;
    }
}

/// Where a region sits on the page.
#[derive(Debug, Clone, Copy)]
struct PageRegion {
    region_id: u8,
    x: u32,
    y: u32,
}

/// The display definition: the coordinate grid, and a window inside it.
#[derive(Debug, Clone, Copy)]
struct Display {
    width: u32,
    height: u32,
    window: Option<(u32, u32)>,
}

impl Default for Display {
    fn default() -> Self {
        Self {
            width: DEFAULT_DISPLAY.0,
            height: DEFAULT_DISPLAY.1,
            window: None,
        }
    }
}

/// The DVB subtitle decoder.
pub struct DvbDecoder {
    regions: HashMap<u8, Region>,
    cluts: HashMap<u8, Clut>,
    page: Vec<PageRegion>,
    page_time_out: u8,
    display: Display,
    /// Regions this display set has composed, which is one of the two things
    /// that keeps a region alive when the budget comes under pressure.
    composed: Vec<u8>,
    /// Segments have arrived since the last display set was emitted.
    open: bool,
    /// The running time the open display set belongs to.
    open_rt: Option<gst::ClockTime>,
    video_size: Option<(u32, u32)>,
    errors: u64,
    budget_resets: u64,
    suppressed: u64,
    /// Whether the display set now open carried a page composition segment.
    /// What tells an empty page the stream asked for apart from an empty page
    /// this decoder has because it lost one.
    saw_page: bool,
    failing: bool,
}

impl Default for DvbDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl DvbDecoder {
    pub fn new() -> Self {
        Self {
            regions: HashMap::new(),
            cluts: HashMap::new(),
            page: Vec::new(),
            page_time_out: 0,
            display: Display::default(),
            composed: Vec::new(),
            open: false,
            open_rt: None,
            video_size: None,
            errors: 0,
            budget_resets: 0,
            suppressed: 0,
            saw_page: false,
            failing: false,
        }
    }

    /// Allocation-budget breaches this decoder has counted, cumulative.
    pub fn budget_resets(&self) -> u64 {
        self.budget_resets
    }

    /// Display sets this decoder had nothing to show for, cumulative.
    ///
    /// The difference between a blank screen the STREAM asked for and a blank
    /// screen this decoder is responsible for. An
    /// empty page is the first: it is how a broadcast takes a subtitle away,
    /// and it is reported as a zero-region update the engine schedules. This
    /// counts the second: a page that named regions and produced no picture,
    /// which after a budget fault or a malformed set is the shape a viewer sees
    /// while the decoder waits to be told where it is.
    ///
    /// **Recovery is stream-gated, and that is the contract rather than a
    /// defect.** A DVB display set is a delta on state this decoder keeps, so
    /// once that state is gone there is nothing to paint into until the stream
    /// rebuilds it, which broadcasts do at every acquisition point and mode
    /// change, typically once a second or two. Until then this counter is what
    /// says the blankness is ours.
    pub fn suppressed_sets(&self) -> u64 {
        self.suppressed
    }

    /// What this decoder is holding, in bytes charged against
    /// [`ALLOCATION_BUDGET`].
    ///
    /// EVERYTHING a stream can grow, not just the obvious buffer. Region pixel
    /// buffers are the big one (they persist across display sets and a stream
    /// can declare as many as it has region ids) but a region composition
    /// segment can also carry ten thousand object placements in its 65 535
    /// bytes, and a page composition as many region entries. Measured, the
    /// omission was 48 MiB of real heap against a `held_bytes()` of 256, an
    /// under-report of nearly two hundred thousand times, all of it in
    /// `placements`.
    ///
    /// Charged rather than measured: a region's pixels are charged the geometry
    /// it DECLARED even before the buffer is filled, which is what makes the
    /// budget a promise about the future rather than a description of the past.
    /// [`Self::allocated_bytes`] is the measurement, and the two are written
    /// separately on purpose.
    pub fn held_bytes(&self) -> u64 {
        let regions: u64 = self.regions.values().map(Region::charge).sum();
        regions
            + (self.page.capacity() * PAGE_REGION_BYTES) as u64
            + (self.cluts.len() * std::mem::size_of::<Clut>()) as u64
            + self.table_bytes()
    }

    /// What this decoder's buffers have actually taken from the allocator.
    ///
    /// The independent half of the cap's contract, and it has to BE
    /// independent: this function was once `{ self.held_bytes() }`, so the
    /// standing invariant `allocated <= held` read `x <= x`, vacuously true at
    /// the one decoder it was written for. It walks
    /// the real capacities now, exactly as `pgs.rs` does, and the difference
    /// from `held_bytes` is the point: this counts what the allocator gave,
    /// that counts what the stream was promised.
    #[doc(hidden)]
    pub fn allocated_bytes(&self) -> u64 {
        let regions: u64 = self.regions.values().map(Region::allocated).sum();
        regions
            + (self.page.capacity() * PAGE_REGION_BYTES) as u64
            + (self.cluts.len() * std::mem::size_of::<Clut>()) as u64
            + self.table_bytes()
    }

    /// What the two maps cost beyond their contents: a hash table holds room
    /// for more entries than it has, and that room is real memory a stream can
    /// make it ask for.
    fn table_bytes(&self) -> u64 {
        // A hash table stores `(key, value)` pairs padded to their alignment,
        // plus a byte of control data per slot. Charging the pair's real size
        // rather than the two fields' sizes is the difference between an
        // estimate that tracks the allocator and one that drifts below it.
        (self.regions.capacity() * (std::mem::size_of::<(u8, Region)>() + 1)
            + self.cluts.capacity() * (std::mem::size_of::<(u8, Clut)>() + 1)) as u64
    }

    fn fail(&mut self, fault: Fault) {
        self.errors += 1;
        if matches!(fault, Fault::Budget(_)) {
            self.budget_resets += 1;
        }
        let reason = fault.reason();
        if self.failing {
            debug!(reason, "dvb: dropping the display set");
        } else {
            warn!(
                reason,
                "dvb: dropping the display set and the state behind it; this line is not \
                 repeated until one decodes"
            );
            self.failing = true;
        }
        // EVERYTHING goes. Unlike VOBSUB, whose packets are self-contained, a
        // DVB display set is a delta on state this decoder has been keeping:
        // if the stream has stopped making sense there is no way to know which
        // half of that state is still true, and painting the next update over
        // a corrupt region is worse than showing nothing until the stream's
        // next mode change or acquisition point rebuilds it.
        self.reset_state();
    }

    /// Drop the page and everything painted for it, keeping what is not the
    /// page's to lose: the video size the ENGINE taught, and the DISPLAY
    /// DEFINITION.
    ///
    /// The display definition survives because it describes the stream's
    /// coordinate system rather than the page drawn in it (the reference's
    /// `delete_state` never touches it either) and because a mode-change page
    /// arrives in the same display set as the definition that precedes it. Not
    /// keeping it meant a mode change reverted a 1920x1080 grid to 720x576 and
    /// dropped the whole set as off-picture.
    fn reset_state(&mut self) {
        self.regions = HashMap::new();
        self.cluts = HashMap::new();
        self.page = Vec::new();
        self.saw_page = false;
        self.page_time_out = 0;
        self.composed = Vec::new();
        self.open = false;
        self.open_rt = None;
    }
}

impl SubpicDecoder for DvbDecoder {
    /// DVB carries everything in band; there is no out-of-band setup.
    fn set_codec_data(&mut self, _data: &[u8]) {}

    fn set_video_size(&mut self, width: u32, height: u32) {
        self.video_size = Some((width, height));
    }

    fn push(&mut self, packet: &BitmapPacket) -> Vec<DisplayUpdate> {
        let Ok(map) = packet.data.map_readable() else {
            self.fail(Fault::Malformed("a packet that could not be mapped"));
            return Vec::new();
        };

        let mut updates = Vec::new();
        // A NEW RUNNING TIME FORCE-TERMINATES whatever is open. Every packet of
        // one display set carries the same time; a different one means the set
        // before it ended without saying so, and its regions are still the
        // right thing to show at the time they were built for.
        if self.open && self.open_rt != Some(packet.rt) {
            debug!(
                open = ?self.open_rt,
                new = %packet.rt,
                "dvb: a display set ended by the arrival of the next one"
            );
            match self.close_set() {
                Ok(Some(update)) => updates.push(update),
                Ok(None) => {}
                // The set that FAILED is the old one; the packet in hand is a
                // new display set at a new time and has nothing to do with it.
                // Returning here instead would throw the new one away too, and
                // the fuzz target found exactly that: a stream could reach a
                // state where the set being terminated always failed, and every
                // packet after it was swallowed on the way in.
                Err(fault) => self.fail(fault),
            }
        }

        match self.consume(map.as_slice(), packet.rt) {
            Ok(more) => {
                updates.extend(more);
                if !updates.is_empty() {
                    self.failing = false;
                }
                updates
            }
            Err(fault) => {
                self.fail(fault);
                updates
            }
        }
    }

    /// Back to just-constructed, the video size and the display definition
    /// included: a reset means the engine re-teaches everything.
    fn reset(&mut self) {
        self.reset_state();
        self.display = Display::default();
        self.video_size = None;
        self.failing = false;
    }

    fn take_decode_errors(&mut self) -> u64 {
        std::mem::take(&mut self.errors)
    }
}

impl DvbDecoder {
    /// One PES data field: the two identifying bytes, then segments.
    fn consume(&mut self, data: &[u8], rt: gst::ClockTime) -> Result<Vec<DisplayUpdate>, Fault> {
        if data.len() < 2 {
            return Err(Fault::Malformed("a packet with no data field in it"));
        }
        if data[0] != DATA_IDENTIFIER || data[1] != SUBTITLE_STREAM_ID {
            // Not a DVB subtitle PES payload at all. The reference refuses the
            // same two bytes, and it is worth refusing rather than skipping:
            // everything after them is being read as segment headers.
            return Err(Fault::Malformed(
                "a packet that is not a DVB subtitle data field",
            ));
        }

        let mut updates = Vec::new();
        let mut at = 2usize;
        while at < data.len() && data[at] == SYNC_BYTE {
            if at + SEGMENT_HEADER > data.len() {
                return Err(Fault::Malformed(
                    "a segment header past the end of its packet",
                ));
            }
            let kind = data[at + 1];
            let _page_id = be16(&data[at + 2..at + 4]);
            let length = usize::from(be16(&data[at + 4..at + 6]));
            let start = at + SEGMENT_HEADER;
            let Some(payload) = data.get(start..start + length) else {
                return Err(Fault::Malformed("a segment longer than its packet"));
            };

            match kind {
                SEGMENT_PAGE => self.page_segment(payload)?,
                SEGMENT_REGION => self.region_segment(payload)?,
                SEGMENT_CLUT => self.clut_segment(payload)?,
                SEGMENT_OBJECT => self.object_segment(payload)?,
                SEGMENT_DISPLAY_DEFINITION => self.display_definition_segment(payload)?,
                SEGMENT_END_OF_DISPLAY_SET => {
                    self.open_rt = Some(rt);
                    if let Some(update) = self.close_set()? {
                        updates.push(update);
                    }
                }
                SEGMENT_STUFFING => {}
                other => debug!(kind = other, "dvb: an unknown segment type"),
            }
            if kind != SEGMENT_END_OF_DISPLAY_SET {
                self.open = true;
                self.open_rt = Some(rt);
            }
            at = start + length;
        }
        Ok(updates)
    }

    /// The page composition segment: how long the page lasts, whether the state
    /// behind it survives, and which regions are on it.
    fn page_segment(&mut self, payload: &[u8]) -> Result<(), Fault> {
        if payload.len() < 2 {
            return Err(Fault::Malformed("a page segment shorter than its header"));
        }
        let page_time_out = payload[0];
        let page_state = (payload[1] >> 2) & 3;

        // THE ONLY FULL RESET. A mode change says the page is being rebuilt
        // from nothing; an acquisition point (state 1) says a receiver may
        // JOIN here, which is not the same thing and must not throw away the
        // regions a joined receiver has just been given.
        if page_state == 2 {
            debug!("dvb: a mode change; the regions and palettes go with it");
            self.reset_state();
        }
        // AFTER the reset, not before. Both of these belong to the display set
        // being read, and the reset is about the sets BEFORE it: assigning the
        // timeout first meant a mode-change page's own timeout was zeroed and
        // then clamped to one second, so an acquisition set that asked for five
        // got one. The display definition is
        // preserved by `reset_state` for the same reason, and the reference's
        // own `delete_state` likewise never touches it.
        self.page_time_out = page_time_out;

        let mut page = Vec::new();
        let mut at = 2usize;
        while at + 6 <= payload.len() {
            page.push(PageRegion {
                region_id: payload[at],
                x: u32::from(be16(&payload[at + 2..at + 4])),
                y: u32::from(be16(&payload[at + 4..at + 6])),
            });
            at += 6;
        }
        debug!(
            regions = page.len(),
            timeout = self.page_time_out,
            "dvb: a page composition"
        );
        self.page = page;
        self.saw_page = true;
        Ok(())
    }

    /// The region composition segment: a region's geometry and palette, and the
    /// objects that paint into it.
    fn region_segment(&mut self, payload: &[u8]) -> Result<(), Fault> {
        if payload.len() < 10 {
            return Err(Fault::Malformed("a region segment shorter than its header"));
        }
        let region_id = payload[0];
        let mut fill = (payload[1] >> 3) & 1 == 1;
        let width = u32::from(be16(&payload[2..4]));
        let height = u32::from(be16(&payload[4..6]));
        // The reference's clamp, and the reason for it: the fields are 16 bits
        // each, so the format permits a region of four gigabytes of indices.
        if width == 0 || height == 0 || width > MAX_REGION_SIDE || height > MAX_REGION_SIDE {
            self.regions.remove(&region_id);
            return Err(Fault::Malformed("a region larger than any display can be"));
        }
        let depth = 1u8 << ((payload[6] >> 2) & 7);
        let depth = if (2..=8).contains(&depth) {
            depth
        } else {
            // The reference substitutes four bits, which is the format's most
            // common depth and a great deal better than refusing a page over
            // one bad nibble.
            debug!(depth, "dvb: an invalid region depth; reading it as 4-bit");
            4
        };
        let clut_id = payload[7];
        let background = if depth == 8 {
            payload[8]
        } else if depth == 4 {
            (payload[9] >> 4) & 15
        } else {
            (payload[9] >> 2) & 3
        };

        let wanted = u64::from(width) * u64::from(height);
        // What the placements in THIS segment will cost, from its length: ten
        // bytes of header and six per entry (eight for a character object, so
        // this is an upper bound). Priced beside the pixels because it is the
        // same segment asking for both, and because a stream that names ten
        // thousand placements per region across two hundred and fifty-six
        // regions is asking for 48 MiB.
        let placement_count = payload.len().saturating_sub(10) / 6;
        let placements = (placement_count * PLACEMENT_BYTES) as u64;
        {
            // THE FIRST ALLOCATION SITE, and the one this whole accounting
            // discipline was built for: region buffers persist, so a stream can
            // keep declaring them. Priced against what is already held, minus
            // whatever this region itself is about to give back.
            let mine = |decoder: &Self| decoder.regions.get(&region_id).map_or(0, Region::charge);
            if self.held_bytes() - mine(self) + wanted + placements > WORKING_BUDGET {
                // THE CHEAPEST RUNG FIRST. The cached expansions are derived
                // data (every one of them can be rebuilt from the indices and
                // the palette) so they go before anything a stream would have
                // to re-send.
                for region in self.regions.values_mut() {
                    region.invalidate();
                }
            }
            if self.held_bytes() - mine(self) + wanted + placements > WORKING_BUDGET {
                // SPILL BEFORE REFUSING. A region nothing displays is still a
                // region this decoder is holding, and a stream that declares
                // two hundred of them and pages two would otherwise fill the
                // budget with pictures for nobody and starve every later
                // display set, the same starvation the PGS object store's own
                // rule prevents, in the one format where the buffers are meant
                // to persist.
                //
                // What survives: whatever the current page composition names,
                // and whatever this display set has composed. Between them that
                // is every region that can appear on screen, including the
                // incremental-paint case where a page names a region composed
                // several display sets ago.
                let page = self.page.clone();
                let composed = self.composed.clone();
                let before = self.regions.len();
                self.regions.retain(|id, _| {
                    *id == region_id
                        || page.iter().any(|on_page| on_page.region_id == *id)
                        || composed.contains(id)
                });
                let spilled = before - self.regions.len();
                if spilled > 0 {
                    debug!(
                        spilled,
                        "dvb: gave up regions no page displays to make room for one it does"
                    );
                }
            }
            if self.held_bytes() - mine(self) + wanted + placements > WORKING_BUDGET {
                return Err(Fault::Budget(
                    "a region past the decoder's allocation budget",
                ));
            }
        }

        let region = self.regions.entry(region_id).or_insert_with(|| Region {
            width,
            height,
            depth,
            clut_id,
            background,
            pixels: Vec::new(),
            placements: Vec::new(),
            expanded: None,
        });
        region.depth = depth;
        region.clut_id = clut_id;
        region.background = background;
        // The composition can change the depth, the palette or the background,
        // any of which changes what the region looks like.
        region.invalidate();
        if region.width != width || region.height != height || region.pixels.len() as u64 != wanted
        {
            // A resize throws the old picture away, and the reference forces a
            // fill for the same reason: the bytes that were there described a
            // different rectangle.
            region.width = width;
            region.height = height;
            // THE OLD BUFFER GOES FIRST. `region.pixels = vec![..]` evaluates
            // its right-hand side before dropping the left, so a resize held
            // both at once: 64 MiB at the peak for a byte-identical resize
            // inside a 32 MiB budget, with no breach counted. Taking it first
            // means the peak is the new buffer
            // alone, which is what the check above priced.
            drop(std::mem::take(&mut region.pixels));
            region.pixels = vec![background; wanted as usize];
            fill = true;
        }
        if fill {
            region.pixels.fill(background);
        }
        // The placements are rebuilt from this segment, exactly as the
        // reference clears the region's display list before reading the new
        // one: a region composition is the complete statement of what paints
        // into it. Rebuilt at the size the segment PRICED, not cleared: a clear
        // keeps the capacity (and one segment can name ten thousand
        // placements, so that capacity is memory a stream keeps for free), and
        // growing it by doubling would take half again as much as was charged,
        // the same gap the PGS object store's own rule prevents, in a list
        // that was not being charged at all.
        region.placements = Vec::with_capacity(placement_count);

        let mut at = 10usize;
        // `at + 6 <= len`, not `len - at >= 6`: a character object advances by
        // eight rather than six, so `at` can pass the end and the subtraction
        // would underflow. Found by the fuzz target, and the reason every walk
        // in this file is written the addition way round.
        while at + 6 <= payload.len() {
            let object_id = be16(&payload[at..at + 2]);
            let object_type = payload[at + 2] >> 6;
            let x = u32::from(be16(&payload[at + 2..at + 4]) & 0x0FFF);
            let y = u32::from(be16(&payload[at + 4..at + 6]) & 0x0FFF);
            at += 6;
            if object_type == 1 || object_type == 2 {
                // A character object carries its two colours here. This decoder
                // does not draw text objects (see `object_segment`), but their
                // two bytes still have to be stepped over.
                at += 2;
            }
            region.placements.push(Placement { object_id, x, y });
        }
        if !self.composed.contains(&region_id) {
            self.composed.push(region_id);
        }
        debug!(region_id, width, height, depth, "dvb: a region composition");
        Ok(())
    }

    /// The CLUT definition segment: palette entries in YCbCr with
    /// transparency, converted to straight-alpha RGBA once, here.
    fn clut_segment(&mut self, payload: &[u8]) -> Result<(), Fault> {
        if payload.len() < 2 {
            return Err(Fault::Malformed("a clut segment shorter than its header"));
        }
        let clut_id = payload[0];
        // A palette definition changes what every region drawn with it looks
        // like, and a region does not record which palette its expansion used.
        // Throwing them all away is one pass over at most 256 pointers and
        // cannot be wrong.
        for region in self.regions.values_mut() {
            region.invalidate();
        }
        // A palette starts as the default one and is overwritten entry by
        // entry: a stream that defines three colours gets the spec's defaults
        // for the rest.
        let clut = self.cluts.entry(clut_id).or_insert_with(Clut::default_clut);

        let mut at = 2usize;
        while at + 2 <= payload.len() {
            let entry_id = payload[at];
            let flags = payload[at + 1];
            let depths = flags & 0xE0;
            if depths == 0 {
                return Err(Fault::Malformed("a clut entry for no depth at all"));
            }
            let full_range = flags & 1 == 1;
            at += 2;

            let (y, cr, cb, transparency) = if full_range {
                let Some(entry) = payload.get(at..at + 4) else {
                    break;
                };
                at += 4;
                (entry[0], entry[1], entry[2], entry[3])
            } else {
                let Some(entry) = payload.get(at..at + 2) else {
                    break;
                };
                at += 2;
                // Six bits of luma, four each of the two chromas and two of
                // transparency, packed across two bytes.
                (
                    entry[0] & 0xFC,
                    (((entry[0] & 0x03) << 2) | ((entry[1] >> 6) & 0x03)) << 4,
                    (entry[1] << 2) & 0xF0,
                    (entry[1] << 6) & 0xC0,
                )
            };
            // Luma zero is the format's way of saying "transparent" whatever
            // the transparency field claims.
            let alpha = if y == 0 { 0 } else { 255 - transparency };
            let colour = ycbcr_to_rgba(y, cb, cr, alpha);

            if depths & 0x80 != 0 && entry_id < 4 {
                clut.two_bit[usize::from(entry_id)] = colour;
            }
            if depths & 0x40 != 0 && entry_id < 16 {
                clut.four_bit[usize::from(entry_id)] = colour;
            }
            if depths & 0x20 != 0 {
                clut.eight_bit[usize::from(entry_id)] = colour;
            }
        }
        Ok(())
    }

    /// The object data segment: the pixels, painted into every region that
    /// claimed this object.
    fn object_segment(&mut self, payload: &[u8]) -> Result<(), Fault> {
        if payload.len() < 3 {
            return Err(Fault::Malformed(
                "an object segment shorter than its header",
            ));
        }
        let object_id = be16(&payload[0..2]);
        let coding_method = (payload[2] >> 2) & 3;
        let non_modifying = (payload[2] >> 1) & 1 == 1;

        if coding_method != 0 {
            // Coding method 1 is "a string of characters", which needs a font
            // and a text renderer inside a bitmap decoder. The reference does
            // not implement it either, and no European broadcaster sends it.
            debug!(
                coding_method,
                "dvb: an object coding this decoder cannot draw"
            );
            return Ok(());
        }
        if payload.len() < 7 {
            return Err(Fault::Malformed("an object segment with no field lengths"));
        }
        let top_len = usize::from(be16(&payload[3..5]));
        let bottom_len = usize::from(be16(&payload[5..7]));
        let body = &payload[7..];
        if top_len + bottom_len > body.len() {
            return Err(Fault::Malformed(
                "an object's fields longer than its segment",
            ));
        }
        let top = &body[..top_len];
        // A bottom field of length zero means the top field's data is used for
        // both: an object of alternating identical lines costs half as much to
        // send.
        let bottom = if bottom_len > 0 {
            &body[top_len..top_len + bottom_len]
        } else {
            top
        };

        // WHICH REGIONS CLAIMED IT. An object nobody composed is refused, which
        // is the reference's rule and the same one the PGS decoder applies: pixels for
        // a region that has not asked for them have nowhere to go.
        let targets: Vec<(u8, Placement)> = self
            .regions
            .iter()
            .flat_map(|(id, region)| {
                region
                    .placements
                    .iter()
                    .filter(|placement| placement.object_id == object_id)
                    .map(move |placement| (*id, *placement))
            })
            .collect();
        if targets.is_empty() {
            debug!(object_id, "dvb: an object no region has composed");
            return Ok(());
        }

        for (region_id, placement) in targets {
            let Some(region) = self.regions.get_mut(&region_id) else {
                continue;
            };
            paint_field(region, &placement, top, 0, non_modifying);
            paint_field(region, &placement, bottom, 1, non_modifying);
        }
        Ok(())
    }

    /// The display definition segment: the coordinate grid, and the window
    /// inside it that region positions are relative to.
    fn display_definition_segment(&mut self, payload: &[u8]) -> Result<(), Fault> {
        if payload.len() < 5 {
            return Err(Fault::Malformed(
                "a display definition shorter than its header",
            ));
        }
        let info = payload[0];
        let width = u32::from(be16(&payload[1..3])) + 1;
        let height = u32::from(be16(&payload[3..5])) + 1;
        if width > MAX_DISPLAY_SIDE || height > MAX_DISPLAY_SIDE {
            // The reference resets to the default grid rather than refusing,
            // and that is the better failure: a broken display definition
            // should cost the stream its custom grid, not its subtitles.
            debug!(
                width,
                height, "dvb: an impossible display size; keeping the default grid"
            );
            self.display = Display::default();
            return Ok(());
        }

        let window = if info & 0x08 != 0 && payload.len() >= 13 {
            let x = u32::from(be16(&payload[5..7]));
            let y = u32::from(be16(&payload[9..11]));
            Some((x, y))
        } else {
            None
        };
        debug!(width, height, ?window, "dvb: a display definition");
        self.display = Display {
            width,
            height,
            window,
        };
        Ok(())
    }

    /// Turn everything the display set has said into what should be on screen.
    fn close_set(&mut self) -> Result<Option<DisplayUpdate>, Fault> {
        let Some(rt) = self.open_rt.take() else {
            return Ok(None);
        };
        self.open = false;
        self.composed.clear();

        if self.page.is_empty() && !std::mem::take(&mut self.saw_page) {
            // BLANK BECAUSE BROKEN, not because the stream said so. There is no
            // page because this decoder has none (either it has not been told
            // one yet or a fault took the one it had) and this display set did
            // not carry a page composition to replace it. Reporting a clear here
            // would tell the engine the stream asked for a blank screen, which
            // is a different fact from the decoder having nothing to draw.
            self.suppressed += 1;
            debug!(%rt, "dvb: nothing to show; the decoder is waiting to be re-taught");
            return Ok(None);
        }
        self.saw_page = false;
        if self.page.is_empty() {
            // AN EMPTY PAGE IS THE CLEAR, and it is how a broadcast takes a
            // subtitle away without waiting for the timeout.
            debug!(%rt, "dvb: an empty page clears the screen");
            return Ok(Some(DisplayUpdate {
                start_rt: rt,
                end_rt: None,
                regions: Vec::new(),
            }));
        }

        // THE SECOND ALLOCATION SITE, priced from the regions' own geometry
        // before a byte of RGBA is taken, and priced per DISTINCT region,
        // because that is what gets allocated: a page may name one region at
        // several positions (the fields allow it), and the expansion below is
        // shared between them by reference count. Charging per page entry
        // instead refused pages that cost nothing extra to draw, which the fuzz
        // target found by building one that named the same region six times.
        let mut distinct: Vec<u8> = Vec::new();
        let mut pixels = 0u64;
        for on_page in &self.page {
            if distinct.contains(&on_page.region_id) {
                continue;
            }
            distinct.push(on_page.region_id);
            if let Some(region) = self.regions.get(&on_page.region_id) {
                pixels =
                    pixels.saturating_add(u64::from(region.width) * u64::from(region.height) * 4);
            }
        }
        if self.held_bytes().saturating_add(pixels) > WORKING_BUDGET {
            return Err(Fault::Budget(
                "a display set past the decoder's allocation budget",
            ));
        }

        let (video_width, video_height) = self
            .video_size
            .unwrap_or((self.display.width, self.display.height));
        let scale_x = f64::from(video_width) / f64::from(self.display.width);
        let scale_y = f64::from(video_height) / f64::from(self.display.height);
        let (window_x, window_y) = self.display.window.unwrap_or((0, 0));

        // THE PAGE, taken out first: the loop below needs the regions mutably
        // (it fills their expansion cache) and cannot hold a borrow of the page
        // at the same time. A page is at most a few dozen entries.
        let page = self.page.clone();
        let mut regions = Vec::with_capacity(page.len());
        for on_page in &page {
            let Some(region) = self.regions.get(&on_page.region_id) else {
                debug!(
                    region_id = on_page.region_id,
                    "dvb: the page names a region no composition has described"
                );
                continue;
            };
            if region.pixels.len() as u64 != u64::from(region.width) * u64::from(region.height) {
                continue;
            }
            let (width, height, depth, clut_id) =
                (region.width, region.height, region.depth, region.clut_id);

            // THE EXPANSION, from the cache when the region has not changed
            // since the last one. This is what makes a re-shown page cost
            // nothing, what makes the pending store's shared-allocation
            // accounting true rather than aspirational, and what lets the
            // engine's no-op adoption check recognise a redelivered page by
            // pointer instead of repainting for it.
            let rgba = match self
                .regions
                .get(&on_page.region_id)
                .and_then(|region| region.expanded.clone())
            {
                Some(cached) => cached,
                None => {
                    let clut = self
                        .cluts
                        .get(&clut_id)
                        .copied()
                        .unwrap_or_else(Clut::default_clut);
                    let region = self.regions.get(&on_page.region_id).expect("just read");
                    let mut expanded = vec![0u8; region.pixels.len() * 4];
                    for (pixel, index) in region.pixels.iter().enumerate() {
                        let colour = clut.entry(depth, *index);
                        expanded[pixel * 4..pixel * 4 + 4].copy_from_slice(&colour);
                    }
                    let shared = Arc::new(expanded);
                    if let Some(region) = self.regions.get_mut(&on_page.region_id) {
                        region.expanded = Some(shared.clone());
                    }
                    shared
                }
            };

            let x = ((f64::from(window_x + on_page.x)) * scale_x).round() as i32;
            let y = ((f64::from(window_y + on_page.y)) * scale_y).round() as i32;
            if x >= video_width as i32 || y >= video_height as i32 {
                debug!(x, y, "dvb: a region placed off the picture");
                continue;
            }
            let render_width = ((f64::from(width) * scale_x).round() as u32)
                .min(video_width - x as u32)
                .max(1);
            let render_height = ((f64::from(height) * scale_y).round() as u32)
                .min(video_height - y as u32)
                .max(1);

            regions.push(BitmapRegion {
                pixels: rgba,
                width,
                height,
                x,
                y,
                render_width,
                render_height,
            });
        }

        if regions.is_empty() {
            // A page that named regions and produced nothing is a stream this
            // decoder has lost track of, but NOT a clear: reporting one would
            // wipe a subtitle that is legitimately on screen.
            self.suppressed += 1;
            debug!(%rt, "dvb: a page whose regions produced no picture");
            return Ok(None);
        }

        // THE TIMEOUT, in seconds, with zero clamped to one: a page that says
        // it lasts no time has not been authored, and the engine needs an end
        // it can schedule.
        let seconds = u64::from(self.page_time_out.max(1));
        let end_rt = rt.checked_add(gst::ClockTime::from_seconds(seconds));
        debug!(%rt, regions = regions.len(), seconds, "dvb: a display set");
        Ok(Some(DisplayUpdate {
            start_rt: rt,
            end_rt,
            regions,
        }))
    }
}

/// Paint one field of an object's pixel data into a region.
///
/// `field` is 0 for the top field and 1 for the bottom. The first line a field
/// paints is the first line of the placement whose parity matches it, and each
/// `end of object line` code advances two lines, which is what interlacing
/// means here: the two fields fill alternating rows of the same region.
fn paint_field(
    region: &mut Region,
    placement: &Placement,
    data: &[u8],
    field: u32,
    non_modifying: bool,
) {
    let mut map2to4: [u8; 4] = [0x0, 0x7, 0x8, 0xF];
    let mut map2to8: [u8; 4] = [0x00, 0x77, 0x88, 0xFF];
    let mut map4to8: [u8; 16] = [
        0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xAA, 0xBB, 0xCC, 0xDD, 0xEE,
        0xFF,
    ];

    let mut x = placement.x;
    let mut y = placement.y;
    if y % 2 != field {
        y += 1;
    }

    let mut at = 0usize;
    while at < data.len() {
        if x >= region.width || y >= region.height {
            debug!(x, y, "dvb: object data past the edge of its region");
            return;
        }
        let code = data[at];
        at += 1;
        match code {
            0x10 => {
                let map: Option<&[u8]> = match region.depth {
                    8 => Some(&map2to8),
                    4 => Some(&map2to4),
                    _ => None,
                };
                let (written, used) =
                    read_2bit_string(region, y, x, &data[at..], map, non_modifying);
                x += written;
                at += used;
            }
            0x11 => {
                if region.depth < 4 {
                    debug!("dvb: a 4-bit string in a 2-bit region");
                    return;
                }
                let map: Option<&[u8]> = (region.depth == 8).then_some(&map4to8);
                let (written, used) =
                    read_4bit_string(region, y, x, &data[at..], map, non_modifying);
                x += written;
                at += used;
            }
            0x12 => {
                if region.depth < 8 {
                    debug!("dvb: an 8-bit string in a shallower region");
                    return;
                }
                let (written, used) = read_8bit_string(region, y, x, &data[at..], non_modifying);
                x += written;
                at += used;
            }
            0x20 => {
                let Some(table) = data.get(at..at + 2) else {
                    return;
                };
                map2to4 = [table[0] >> 4, table[0] & 0xF, table[1] >> 4, table[1] & 0xF];
                at += 2;
            }
            0x21 => {
                let Some(table) = data.get(at..at + 4) else {
                    return;
                };
                map2to8.copy_from_slice(table);
                at += 4;
            }
            0x22 => {
                let Some(table) = data.get(at..at + 16) else {
                    return;
                };
                map4to8.copy_from_slice(table);
                at += 16;
            }
            0xF0 => {
                x = placement.x;
                y += 2;
            }
            other => {
                debug!(code = other, "dvb: an unknown pixel data block");
                return;
            }
        }
    }
}

/// Write one run into a region's line, honouring the non-modifying rule.
///
/// `non_modifying` with index 1 is the format's transparency-preserving
/// pseudo-colour: those pixels keep whatever was already there, which is how a
/// display set repaints part of a region without redrawing the rest.
fn write_run(region: &mut Region, y: u32, x: u32, run: u32, index: u8, non_modifying: bool) -> u32 {
    let run = run.min(region.width.saturating_sub(x));
    if run == 0 {
        return 0;
    }
    if non_modifying && index == 1 {
        return run;
    }
    region.invalidate();
    let start = (y as usize) * (region.width as usize) + x as usize;
    let end = start + run as usize;
    if end <= region.pixels.len() {
        region.pixels[start..end].fill(index);
    }
    run
}

/// A big-endian bit reader over a byte slice, in the shape the three run-length
/// codings need: read n bits, or stop.
struct Bits<'a> {
    data: &'a [u8],
    at: usize,
}

impl<'a> Bits<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, at: 0 }
    }

    fn remaining(&self) -> usize {
        self.data.len() * 8 - self.at
    }

    fn read(&mut self, count: usize) -> Option<u32> {
        if self.remaining() < count {
            return None;
        }
        let mut value = 0u32;
        for _ in 0..count {
            let byte = self.data[self.at / 8];
            let bit = (byte >> (7 - (self.at % 8))) & 1;
            value = (value << 1) | u32::from(bit);
            self.at += 1;
        }
        Some(value)
    }

    /// Bytes consumed, rounded up: every pixel string ends on a byte boundary.
    fn bytes_used(&self) -> usize {
        self.at.div_ceil(8)
    }
}

/// The 2-bit run-length coding. Returns `(pixels written, bytes consumed)`.
fn read_2bit_string(
    region: &mut Region,
    y: u32,
    x: u32,
    data: &[u8],
    map: Option<&[u8]>,
    non_modifying: bool,
) -> (u32, usize) {
    let mut bits = Bits::new(data);
    let mut written = 0u32;
    while bits.remaining() > 1 {
        let Some(code) = bits.read(2) else { break };
        let (run, index) = if code != 0 {
            (1u32, code as u8)
        } else {
            let Some(switch_1) = bits.read(1) else { break };
            if switch_1 == 1 {
                let (Some(run), Some(index)) = (bits.read(3), bits.read(2)) else {
                    break;
                };
                (run + 3, index as u8)
            } else {
                let Some(switch_2) = bits.read(1) else { break };
                if switch_2 == 1 {
                    (1, 0)
                } else {
                    let Some(switch_3) = bits.read(2) else { break };
                    match switch_3 {
                        0 => break,
                        1 => (2, 0),
                        2 => {
                            let (Some(run), Some(index)) = (bits.read(4), bits.read(2)) else {
                                break;
                            };
                            (run + 12, index as u8)
                        }
                        _ => {
                            let (Some(run), Some(index)) = (bits.read(8), bits.read(2)) else {
                                break;
                            };
                            (run + 29, index as u8)
                        }
                    }
                }
            }
        };
        let index = map.map_or(index, |table| table[usize::from(index) & 3]);
        written += write_run(region, y, x + written, run, index, non_modifying);
    }
    (written, bits.bytes_used())
}

/// The 4-bit run-length coding.
fn read_4bit_string(
    region: &mut Region,
    y: u32,
    x: u32,
    data: &[u8],
    map: Option<&[u8]>,
    non_modifying: bool,
) -> (u32, usize) {
    let mut bits = Bits::new(data);
    let mut written = 0u32;
    while bits.remaining() > 3 {
        let Some(code) = bits.read(4) else { break };
        let (run, index) = if code != 0 {
            (1u32, code as u8)
        } else {
            let Some(switch_1) = bits.read(1) else { break };
            if switch_1 == 0 {
                let Some(run) = bits.read(3) else { break };
                if run == 0 {
                    break;
                }
                (run + 2, 0)
            } else {
                let Some(switch_2) = bits.read(1) else { break };
                if switch_2 == 0 {
                    let (Some(run), Some(index)) = (bits.read(2), bits.read(4)) else {
                        break;
                    };
                    (run + 4, index as u8)
                } else {
                    let Some(switch_3) = bits.read(2) else { break };
                    match switch_3 {
                        0 => (1, 0),
                        1 => (2, 0),
                        2 => {
                            let (Some(run), Some(index)) = (bits.read(4), bits.read(4)) else {
                                break;
                            };
                            (run + 9, index as u8)
                        }
                        _ => {
                            let (Some(run), Some(index)) = (bits.read(8), bits.read(4)) else {
                                break;
                            };
                            (run + 25, index as u8)
                        }
                    }
                }
            }
        };
        let index = map.map_or(index, |table| table[usize::from(index) & 15]);
        written += write_run(region, y, x + written, run, index, non_modifying);
    }
    (written, bits.bytes_used())
}

/// The 8-bit run-length coding. No map table: eight bits already index the
/// deepest palette the format has.
fn read_8bit_string(
    region: &mut Region,
    y: u32,
    x: u32,
    data: &[u8],
    non_modifying: bool,
) -> (u32, usize) {
    let mut bits = Bits::new(data);
    let mut written = 0u32;
    while bits.remaining() > 7 {
        let Some(code) = bits.read(8) else { break };
        let (run, index) = if code != 0 {
            (1u32, code as u8)
        } else {
            let Some(switch_1) = bits.read(1) else { break };
            if switch_1 == 0 {
                let Some(run) = bits.read(7) else { break };
                if run == 0 {
                    break;
                }
                (run, 0)
            } else {
                let (Some(run), Some(index)) = (bits.read(7), bits.read(8)) else {
                    break;
                };
                (run, index as u8)
            }
        };
        written += write_run(region, y, x + written, run, index, non_modifying);
    }
    (written, bits.bytes_used())
}

/// One CLUT entry, from the wire's YCbCr + transparency to the renderer's
/// straight-alpha RGBA.
///
/// **BT.601, limited range**, and this is the one conversion the reference
/// could not supply: it keeps its palettes in AYUV and lets its compositor
/// blend there, so it never converts back.
///
/// Not "because DVB is standard definition": plenty of DVB services are HD,
/// and their video is BT.709. The subtitle CLUT is a separate matter: ETSI EN
/// 300 743 defines its entries as Y, Cr, Cb without ever naming a matrix, the
/// authoring tools and the receivers that grew up around it settled on BT.601,
/// and a receiver that switched matrices with the service resolution would draw
/// the same broadcaster's subtitles in two different colours. PGS is BT.709 for
/// the opposite kind of reason: its graphics plane is defined against the
/// disc's own video, which is BT.709 by the Blu-ray specification.
///
/// **Straight alpha**, like both of its neighbours.
fn ycbcr_to_rgba(y: u8, cb: u8, cr: u8, alpha: u8) -> Rgba {
    let (y, cb, cr) = (i32::from(y), i32::from(cb), i32::from(cr));
    let r = (298 * y + 409 * cr - 57120) >> 8;
    let g = (298 * y - 100 * cb - 208 * cr + 34656) >> 8;
    let b = (298 * y + 516 * cb - 70816) >> 8;
    [
        r.clamp(0, 255) as u8,
        g.clamp(0, 255) as u8,
        b.clamp(0, 255) as u8,
        alpha,
    ]
}

fn be16(bytes: &[u8]) -> u16 {
    u16::from_be_bytes([bytes[0], bytes[1]])
}

/// Hand-crafted DVB display sets: PES data fields, segment by segment.
///
/// Doc-hidden and shipped for the same reason the other two decoders' fixtures
/// are: this file's vectors, the fuzz target's known-good tail and its seed
/// corpus all need the same bytes, and a `cfg(test)` module is invisible to
/// `tests/` and `fuzz/`.
#[doc(hidden)]
pub mod fixtures {
    use super::{
        DATA_IDENTIFIER, SEGMENT_CLUT, SEGMENT_DISPLAY_DEFINITION, SEGMENT_END_OF_DISPLAY_SET,
        SEGMENT_OBJECT, SEGMENT_PAGE, SEGMENT_REGION, SUBTITLE_STREAM_ID, SYNC_BYTE,
    };

    /// `[0x0f][type][page id][length][payload]`.
    pub fn segment(kind: u8, payload: &[u8]) -> Vec<u8> {
        let mut out = vec![SYNC_BYTE, kind, 0x00, 0x01];
        out.extend_from_slice(&(payload.len() as u16).to_be_bytes());
        out.extend_from_slice(payload);
        out
    }

    /// A PES data field carrying the given segments.
    pub fn data_field(segments: &[Vec<u8>]) -> Vec<u8> {
        let mut out = vec![DATA_IDENTIFIER, SUBTITLE_STREAM_ID];
        for part in segments {
            out.extend_from_slice(part);
        }
        out
    }

    /// A page composition: the timeout in seconds, the page state, and the
    /// regions on the page with their positions.
    pub fn page(timeout: u8, state: u8, regions: &[(u8, u16, u16)]) -> Vec<u8> {
        let mut payload = vec![timeout, (state & 3) << 2];
        for (id, x, y) in regions {
            payload.push(*id);
            payload.push(0);
            payload.extend_from_slice(&x.to_be_bytes());
            payload.extend_from_slice(&y.to_be_bytes());
        }
        segment(SEGMENT_PAGE, &payload)
    }

    /// A region composition: geometry, depth, palette, background, and the
    /// objects that paint into it.
    ///
    /// Eight parameters, which is one past what clippy likes and exactly what
    /// the segment carries: grouping them into a struct would put a layer of
    /// naming between a vector and the bytes it is asserting about, which is
    /// the opposite of what a fixture is for.
    #[allow(clippy::too_many_arguments)]
    pub fn region(
        id: u8,
        width: u16,
        height: u16,
        depth: u8,
        clut_id: u8,
        background: u8,
        fill: bool,
        objects: &[(u16, u16, u16)],
    ) -> Vec<u8> {
        // depth is stored as log2: 2 -> 1, 4 -> 2, 8 -> 3.
        let depth_code = match depth {
            2 => 1u8,
            8 => 3,
            _ => 2,
        };
        let mut payload = vec![id, if fill { 1 << 3 } else { 0 }];
        payload.extend_from_slice(&width.to_be_bytes());
        payload.extend_from_slice(&height.to_be_bytes());
        payload.push(depth_code << 2);
        payload.push(clut_id);
        // The 8-bit background, then the 4-bit and 2-bit ones packed together.
        payload.push(if depth == 8 { background } else { 0 });
        payload.push(match depth {
            4 => background << 4,
            2 => background << 2,
            _ => 0,
        });
        for (object_id, x, y) in objects {
            payload.extend_from_slice(&object_id.to_be_bytes());
            payload.extend_from_slice(&x.to_be_bytes());
            payload.extend_from_slice(&y.to_be_bytes());
        }
        segment(SEGMENT_REGION, &payload)
    }

    /// A CLUT definition with full-range entries: `(entry, Y, Cr, Cb,
    /// transparency)`, applied at every depth.
    pub fn clut(id: u8, entries: &[(u8, u8, u8, u8, u8)]) -> Vec<u8> {
        let mut payload = vec![id, 0];
        for (entry, y, cr, cb, transparency) in entries {
            payload.push(*entry);
            // All three depths, full range.
            payload.push(0xE0 | 1);
            payload.extend_from_slice(&[*y, *cr, *cb, *transparency]);
        }
        segment(SEGMENT_CLUT, &payload)
    }

    /// An object data segment carrying one top field and one bottom field of
    /// already-encoded pixel data blocks.
    pub fn object(id: u16, top: &[u8], bottom: &[u8]) -> Vec<u8> {
        let mut payload = id.to_be_bytes().to_vec();
        payload.push(0); // coding method 0, colours modifiable
        payload.extend_from_slice(&(top.len() as u16).to_be_bytes());
        payload.extend_from_slice(&(bottom.len() as u16).to_be_bytes());
        payload.extend_from_slice(top);
        payload.extend_from_slice(bottom);
        segment(SEGMENT_OBJECT, &payload)
    }

    /// A display definition segment: the grid, and optionally a window.
    pub fn display_definition(
        width: u16,
        height: u16,
        window: Option<(u16, u16, u16, u16)>,
    ) -> Vec<u8> {
        let mut payload = vec![if window.is_some() { 0x08 } else { 0x00 }];
        payload.extend_from_slice(&(width - 1).to_be_bytes());
        payload.extend_from_slice(&(height - 1).to_be_bytes());
        if let Some((x, x_end, y, y_end)) = window {
            payload.extend_from_slice(&x.to_be_bytes());
            payload.extend_from_slice(&x_end.to_be_bytes());
            payload.extend_from_slice(&y.to_be_bytes());
            payload.extend_from_slice(&y_end.to_be_bytes());
        }
        segment(SEGMENT_DISPLAY_DEFINITION, &payload)
    }

    pub fn end_of_display_set() -> Vec<u8> {
        segment(SEGMENT_END_OF_DISPLAY_SET, &[])
    }

    /// A bit writer, for building the run-length codings by hand.
    #[derive(Default)]
    pub struct BitWriter {
        bytes: Vec<u8>,
        bits: usize,
    }

    impl BitWriter {
        pub fn write(&mut self, value: u32, count: usize) {
            for step in (0..count).rev() {
                if self.bits.is_multiple_of(8) {
                    self.bytes.push(0);
                }
                if (value >> step) & 1 == 1 {
                    let index = self.bits / 8;
                    self.bytes[index] |= 1 << (7 - (self.bits % 8));
                }
                self.bits += 1;
            }
        }

        /// Pad to the next byte, which every pixel string does.
        pub fn finish(mut self) -> Vec<u8> {
            while !self.bits.is_multiple_of(8) {
                self.write(0, 1);
            }
            std::mem::take(&mut self.bytes)
        }
    }

    /// A 2-bit pixel string: runs of `(index, length)`, then the end code.
    ///
    /// The codings can only express certain run lengths directly, so a run
    /// this builder cannot say in one code is split into ones it can. That is
    /// what a real encoder does too, and it keeps the vectors readable: a test
    /// asks for "eight pixels of entry 1" without knowing that eight is not a
    /// number the 4-bit coding has a code for.
    pub fn two_bit_string(runs: &[(u8, u32)]) -> Vec<u8> {
        let mut bits = BitWriter::default();
        for (index, length) in runs {
            let mut left = *length;
            while left > 0 {
                let index = u32::from(*index & 3);
                if index != 0 && left >= 29 {
                    let take = left.min(284);
                    bits.write(0, 2);
                    bits.write(0, 1);
                    bits.write(0, 1);
                    bits.write(3, 2);
                    bits.write(take - 29, 8);
                    bits.write(index, 2);
                    left -= take;
                } else if index != 0 && left >= 12 {
                    let take = left.min(27);
                    bits.write(0, 2);
                    bits.write(0, 1);
                    bits.write(0, 1);
                    bits.write(2, 2);
                    bits.write(take - 12, 4);
                    bits.write(index, 2);
                    left -= take;
                } else if left >= 3 {
                    let take = left.min(10);
                    bits.write(0, 2);
                    bits.write(1, 1);
                    bits.write(take - 3, 3);
                    bits.write(index, 2);
                    left -= take;
                } else if index != 0 {
                    bits.write(index, 2);
                    left -= 1;
                } else if left >= 2 {
                    bits.write(0, 2);
                    bits.write(0, 1);
                    bits.write(0, 1);
                    bits.write(1, 2);
                    left -= 2;
                } else {
                    bits.write(0, 2);
                    bits.write(0, 1);
                    bits.write(1, 1);
                    left -= 1;
                }
            }
        }
        // end of 2-bit string
        bits.write(0, 2);
        bits.write(0, 1);
        bits.write(0, 1);
        bits.write(0, 2);
        let mut out = vec![0x10];
        out.extend_from_slice(&bits.finish());
        out
    }

    /// A 4-bit pixel string.
    pub fn four_bit_string(runs: &[(u8, u32)]) -> Vec<u8> {
        let mut bits = BitWriter::default();
        for (index, length) in runs {
            let mut left = *length;
            while left > 0 {
                let index = u32::from(*index & 15);
                if index != 0 && left >= 25 {
                    let take = left.min(280);
                    bits.write(0, 4);
                    bits.write(1, 1);
                    bits.write(1, 1);
                    bits.write(3, 2);
                    bits.write(take - 25, 8);
                    bits.write(index, 4);
                    left -= take;
                } else if index != 0 && left >= 9 {
                    let take = left.min(24);
                    bits.write(0, 4);
                    bits.write(1, 1);
                    bits.write(1, 1);
                    bits.write(2, 2);
                    bits.write(take - 9, 4);
                    bits.write(index, 4);
                    left -= take;
                } else if index != 0 && left >= 4 {
                    let take = left.min(7);
                    bits.write(0, 4);
                    bits.write(1, 1);
                    bits.write(0, 1);
                    bits.write(take - 4, 2);
                    bits.write(index, 4);
                    left -= take;
                } else if index == 0 && left >= 3 {
                    let take = left.min(9);
                    bits.write(0, 4);
                    bits.write(0, 1);
                    bits.write(take - 2, 3);
                    left -= take;
                } else if index != 0 {
                    bits.write(index, 4);
                    left -= 1;
                } else if left >= 2 {
                    bits.write(0, 4);
                    bits.write(1, 1);
                    bits.write(1, 1);
                    bits.write(1, 2);
                    left -= 2;
                } else {
                    bits.write(0, 4);
                    bits.write(1, 1);
                    bits.write(1, 1);
                    bits.write(0, 2);
                    left -= 1;
                }
            }
        }
        // end of 4-bit string: switch_1 == 0 with a zero run
        bits.write(0, 4);
        bits.write(0, 1);
        bits.write(0, 3);
        let mut out = vec![0x11];
        out.extend_from_slice(&bits.finish());
        out
    }

    /// An 8-bit pixel string.
    pub fn eight_bit_string(runs: &[(u8, u32)]) -> Vec<u8> {
        let mut bits = BitWriter::default();
        for (index, length) in runs {
            let mut left = *length;
            while left > 0 {
                if left == 1 && *index != 0 {
                    bits.write(u32::from(*index), 8);
                    left -= 1;
                } else if *index == 0 {
                    let take = left.min(127);
                    bits.write(0, 8);
                    bits.write(0, 1);
                    bits.write(take, 7);
                    left -= take;
                } else {
                    let take = left.min(127);
                    bits.write(0, 8);
                    bits.write(1, 1);
                    bits.write(take, 7);
                    bits.write(u32::from(*index), 8);
                    left -= take;
                }
            }
        }
        bits.write(0, 8);
        bits.write(0, 1);
        bits.write(0, 7);
        let mut out = vec![0x12];
        out.extend_from_slice(&bits.finish());
        out
    }

    /// The end-of-object-line code: the next data goes two lines down.
    pub fn end_of_line() -> Vec<u8> {
        vec![0xF0]
    }

    /// THE MINIMAL DISPLAY SET: a 4x2 region at (100, 200) on the default grid,
    /// 4-bit, one object whose two fields differ by construction: the top
    /// field draws palette entry 1 and the bottom entry 2, so a decoder that
    /// reads one field twice cannot pass.
    pub fn minimal_display_set() -> Vec<u8> {
        let top = four_bit_string(&[(1, 4)]);
        let bottom = four_bit_string(&[(2, 4)]);
        data_field(&[
            page(5, 0, &[(1, 100, 200)]),
            region(1, 4, 2, 4, 1, 0, true, &[(7, 0, 0)]),
            clut(
                1,
                &[
                    // entry 1: white, opaque; entry 2: mid grey, opaque
                    (1, 235, 128, 128, 0),
                    (2, 128, 128, 128, 0),
                ],
            ),
            object(7, &top, &bottom),
            end_of_display_set(),
        ])
    }

    /// THE ACQUISITION DISPLAY SET: the same picture as
    /// [`minimal_display_set`], preceded by everything needed to interpret it
    /// from nothing: a display definition naming the standard grid, and a page
    /// whose state is a MODE CHANGE.
    ///
    /// This is what a broadcast sends so a receiver that has just tuned in can
    /// start, and it is what the fuzz target feeds at the end of every run: a
    /// decoder that cannot draw after a mode change is wedged, whereas one that
    /// draws nothing because the stream set a one-pixel-high display grid is
    /// obeying the stream. The distinction matters, since the second is how the
    /// recovery invariant first fired, and it was the invariant that was wrong.
    pub fn acquisition_display_set() -> Vec<u8> {
        let top = four_bit_string(&[(1, 4)]);
        let bottom = four_bit_string(&[(2, 4)]);
        data_field(&[
            // A NON-DEFAULT grid, deliberately: with the default one, a mode
            // change that wiped the display definition was a no-op and nothing
            // saw it.
            display_definition(1024, 576, None),
            page(5, 2, &[(1, 100, 200)]),
            region(1, 4, 2, 4, 1, 0, true, &[(7, 0, 0)]),
            clut(1, &[(1, 235, 128, 128, 0), (2, 128, 128, 128, 0)]),
            object(7, &top, &bottom),
            end_of_display_set(),
        ])
    }

    /// THE GROUNDED DISPLAY SET: the same picture as [`minimal_display_set`]
    /// with the grid it is drawn on stated, and a page state of 1, an
    /// ACQUISITION POINT, which resets nothing.
    ///
    /// The fuzz target's non-resetting tail. [`acquisition_display_set`] can be
    /// held to a picture because its mode change throws the fuzzer's state away
    /// first, which is exactly why it proves nothing about that state; the
    /// minimal set keeps the state but cannot be held to a picture, because the
    /// fuzzer may have set a display grid that legitimately puts it off screen.
    /// This one keeps the state AND names its own grid, so it can be held to
    /// both.
    pub fn grounded_display_set() -> Vec<u8> {
        let top = four_bit_string(&[(1, 4)]);
        let bottom = four_bit_string(&[(2, 4)]);
        data_field(&[
            display_definition(1024, 576, None),
            page(5, 1, &[(1, 100, 200)]),
            region(1, 4, 2, 4, 1, 0, true, &[(7, 0, 0)]),
            clut(1, &[(1, 235, 128, 128, 0), (2, 128, 128, 128, 0)]),
            object(7, &top, &bottom),
            end_of_display_set(),
        ])
    }

    /// The display set that takes the page away: a page with no regions on it.
    pub fn empty_page() -> Vec<u8> {
        data_field(&[page(5, 0, &[]), end_of_display_set()])
    }

    /// Every vector above, named, for a fuzz seed corpus.
    pub fn seed_corpus() -> Vec<(&'static str, Vec<u8>)> {
        vec![
            ("minimal", minimal_display_set()),
            ("acquisition", acquisition_display_set()),
            ("grounded", grounded_display_set()),
            ("empty-page", empty_page()),
            (
                "display-definition",
                data_field(&[
                    display_definition(1920, 1080, Some((16, 1903, 8, 1071))),
                    page(3, 0, &[(1, 10, 20)]),
                    region(1, 8, 4, 8, 2, 0, true, &[(9, 0, 0)]),
                    clut(2, &[(3, 200, 100, 100, 0)]),
                    object(
                        9,
                        &eight_bit_string(&[(3, 8)]),
                        &eight_bit_string(&[(3, 8)]),
                    ),
                    end_of_display_set(),
                ]),
            ),
            (
                "two-bit-region",
                data_field(&[
                    page(1, 0, &[(4, 0, 0)]),
                    region(4, 8, 2, 2, 0, 0, true, &[(1, 0, 0)]),
                    object(
                        1,
                        &two_bit_string(&[(1, 4), (2, 4)]),
                        &two_bit_string(&[(3, 8)]),
                    ),
                    end_of_display_set(),
                ]),
            ),
        ]
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::{fixtures::*, *};
    use crate::{cue::CueEngine, subpic::BitmapFormat, video::OverlaySpace};

    fn packet_at(bytes: &[u8], rt_ms: u64) -> BitmapPacket {
        BitmapPacket {
            format: BitmapFormat::Dvb,
            data: gst::Buffer::from_slice(bytes.to_vec()),
            codec_data: None,
            rt: gst::ClockTime::from_mseconds(rt_ms),
            duration: None,
        }
    }

    fn taught() -> DvbDecoder {
        let mut decoder = DvbDecoder::new();
        decoder.set_video_size(720, 576);
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

    /// One display set in, one region out, with its schedule and its colours.
    ///
    /// The two rows come from DIFFERENT fields by construction: the top field
    /// draws palette entry 1 and the bottom entry 2, so a decoder that reads
    /// one field twice, or swaps them, cannot pass. The alpha is straight: the
    /// CLUT's transparency byte is zero, which is fully opaque, and the colours
    /// come out of the BT.601 conversion unscaled.
    #[test]
    fn a_minimal_display_set_becomes_one_region() {
        gst::init().unwrap();
        let mut decoder = taught();

        let updates = decoder.push(&packet_at(&minimal_display_set(), 1_000));
        assert_eq!(updates.len(), 1, "one display set is one update");
        let update = &updates[0];
        assert_eq!(update.start_rt, gst::ClockTime::from_mseconds(1_000));
        assert_eq!(
            update.end_rt,
            Some(gst::ClockTime::from_mseconds(6_000)),
            "the page's five-second timeout is the end"
        );
        assert_eq!(update.regions.len(), 1);

        let region = &update.regions[0];
        assert_eq!((region.width, region.height), (4, 2));
        assert_eq!((region.x, region.y), (100, 200), "the page's own position");
        let top = pixel(region, 0, 0);
        let bottom = pixel(region, 0, 1);
        assert_eq!(top[3], 255, "an opaque entry");
        assert!(
            top[0] > 200 && top[1] > 200 && top[2] > 200,
            "entry 1 is white: {top:?}"
        );
        assert!(
            bottom[0] > 100 && bottom[0] < 160,
            "entry 2 is mid grey: {bottom:?}"
        );
        assert_ne!(top, bottom, "both rows decoded from the same field");
        assert_eq!(decoder.take_decode_errors(), 0);
        assert_eq!(
            decoder.suppressed_sets(),
            0,
            "a set that drew a picture was counted as a blank the decoder owed"
        );
    }

    /// The three run-length codings, each into a region of its own depth.
    #[test]
    fn all_three_pixel_depths_decode() {
        gst::init().unwrap();

        for (depth, string) in [
            (2u8, two_bit_string(&[(1, 4)])),
            (4, four_bit_string(&[(1, 4)])),
            (8, eight_bit_string(&[(1, 4)])),
        ] {
            let mut decoder = taught();
            let bytes = data_field(&[
                page(2, 0, &[(1, 0, 0)]),
                region(1, 4, 2, depth, 1, 0, true, &[(5, 0, 0)]),
                clut(1, &[(1, 235, 128, 128, 0)]),
                object(5, &string, &string),
                end_of_display_set(),
            ]);
            let updates = decoder.push(&packet_at(&bytes, 0));
            assert_eq!(updates.len(), 1, "{depth}-bit: no display set came out");
            let region = &updates[0].regions[0];
            assert_eq!(
                pixel(region, 0, 0)[3],
                255,
                "{depth}-bit: the run was not drawn"
            );
            // THE LAST PIXEL OF THE RUN, which is what makes the run LENGTH
            // load-bearing: a coding that decoded the colour but not the length
            // would draw one pixel and pass on the first assertion alone.
            assert_eq!(
                pixel(region, 3, 0)[3],
                255,
                "{depth}-bit: the run's length was decoded wrong"
            );
            assert_eq!(decoder.take_decode_errors(), 0, "{depth}-bit");
        }
    }

    /// The default CLUT is used when a region names a palette the stream never
    /// defined, and it is the spec's, not an empty one.
    #[test]
    fn an_undefined_palette_falls_back_to_the_specs_own() {
        gst::init().unwrap();
        let mut decoder = taught();

        // Region 1 names clut 9; no clut segment defines it.
        let bytes = data_field(&[
            page(2, 0, &[(1, 0, 0)]),
            region(1, 4, 2, 4, 9, 0, true, &[(5, 0, 0)]),
            object(5, &four_bit_string(&[(1, 4)]), &four_bit_string(&[(1, 4)])),
            end_of_display_set(),
        ]);
        let updates = decoder.push(&packet_at(&bytes, 0));
        let region = &updates[0].regions[0];
        assert_eq!(
            pixel(region, 0, 0),
            [255, 0, 0, 255],
            "the default 4-bit palette's entry 1 is opaque red"
        );
        assert_eq!(
            pixel(region, 0, 0)[3],
            255,
            "the default palette's visible entries are opaque"
        );
    }

    /// A display definition segment moves the grid AND the window, and both
    /// reach the placement.
    #[test]
    fn the_display_definition_sets_the_grid_and_the_window() {
        gst::init().unwrap();

        let placed = |dds: Option<Vec<u8>>| {
            let mut decoder = DvbDecoder::new();
            decoder.set_video_size(1920, 1080);
            let mut segments = Vec::new();
            if let Some(dds) = dds {
                segments.push(dds);
            }
            segments.extend([
                page(2, 0, &[(1, 100, 50)]),
                region(1, 8, 4, 4, 1, 0, true, &[(5, 0, 0)]),
                clut(1, &[(1, 235, 128, 128, 0)]),
                object(5, &four_bit_string(&[(1, 8)]), &four_bit_string(&[(1, 8)])),
                end_of_display_set(),
            ]);
            let updates = decoder.push(&packet_at(&data_field(&segments), 0));
            let region = &updates[0].regions[0];
            (
                region.x,
                region.y,
                region.render_width,
                region.render_height,
            )
        };

        // No DDS: the default 720x576 grid, stretched onto 1920x1080.
        assert_eq!(placed(None), (267, 94, 21, 8));
        // A DDS naming the video's own size: no stretch at all.
        assert_eq!(
            placed(Some(display_definition(1920, 1080, None))),
            (100, 50, 8, 4)
        );
        // ...and with a window, whose origin the region's position is relative
        // to.
        assert_eq!(
            placed(Some(display_definition(
                1920,
                1080,
                Some((16, 1903, 8, 1071))
            ))),
            (116, 58, 8, 4)
        );
    }

    /// `page_time_out` is in SECONDS, and a page claiming zero gets one.
    #[test]
    fn the_page_timeout_is_seconds_and_zero_becomes_one() {
        gst::init().unwrap();

        for (timeout, expected) in [(0u8, 1_000u64), (1, 1_000), (7, 7_000), (255, 255_000)] {
            let mut decoder = taught();
            let bytes = data_field(&[
                page(timeout, 0, &[(1, 0, 0)]),
                region(1, 4, 2, 4, 1, 0, true, &[(5, 0, 0)]),
                clut(1, &[(1, 235, 128, 128, 0)]),
                object(5, &four_bit_string(&[(1, 4)]), &four_bit_string(&[(1, 4)])),
                end_of_display_set(),
            ]);
            let updates = decoder.push(&packet_at(&bytes, 500));
            assert_eq!(
                updates[0].end_rt,
                Some(gst::ClockTime::from_mseconds(500 + expected)),
                "timeout {timeout}"
            );
        }
    }

    /// INCREMENTAL PAINT, which is what makes this format expensive: a second
    /// display set repaints part of a region and re-shows it, and the part it
    /// did not touch is still there.
    #[test]
    fn a_second_display_set_paints_into_the_region_it_left_behind() {
        gst::init().unwrap();
        let mut decoder = taught();

        // First set: an 8x2 region, all of it entry 1.
        let first = data_field(&[
            page(5, 0, &[(1, 0, 0)]),
            region(1, 8, 2, 4, 1, 0, true, &[(5, 0, 0)]),
            clut(1, &[(1, 235, 128, 128, 0), (2, 81, 240, 90, 0)]),
            object(5, &four_bit_string(&[(1, 8)]), &four_bit_string(&[(1, 8)])),
            end_of_display_set(),
        ]);
        let updates = decoder.push(&packet_at(&first, 0));
        let entry_one = pixel(&updates[0].regions[0], 7, 0);
        assert_eq!(entry_one[3], 255);

        // Second set: the SAME region, not re-declared as filled, with an
        // object that paints four pixels of entry 2 at the left.
        let second = data_field(&[
            page(5, 0, &[(1, 0, 0)]),
            region(1, 8, 2, 4, 1, 0, false, &[(6, 0, 0)]),
            object(6, &four_bit_string(&[(2, 4)]), &four_bit_string(&[(2, 4)])),
            end_of_display_set(),
        ]);
        let updates = decoder.push(&packet_at(&second, 1_000));
        let painted = &updates[0].regions[0];
        assert_ne!(
            pixel(painted, 0, 0),
            entry_one,
            "the second set's object never painted"
        );
        assert_eq!(
            pixel(painted, 7, 0),
            entry_one,
            "the part the second set did not touch was lost: this format's regions PERSIST"
        );
    }

    /// `page_state == 2` is a mode change, and the ONLY thing that empties the
    /// regions. An acquisition point must not.
    #[test]
    fn only_a_mode_change_resets_the_persistent_regions() {
        gst::init().unwrap();
        let mut decoder = taught();

        let build = data_field(&[
            page(5, 0, &[(1, 0, 0)]),
            region(1, 8, 2, 4, 1, 0, true, &[(5, 0, 0)]),
            clut(1, &[(1, 235, 128, 128, 0)]),
            object(5, &four_bit_string(&[(1, 8)]), &four_bit_string(&[(1, 8)])),
            end_of_display_set(),
        ]);
        decoder.push(&packet_at(&build, 0));
        let held = decoder.held_bytes();
        assert!(held > 0, "the region buffer is what is being held");

        // An ACQUISITION POINT keeps everything: a receiver may join here, and
        // throwing the regions away would blank a page it just built.
        let acquisition = data_field(&[page(5, 1, &[(1, 0, 0)]), end_of_display_set()]);
        let updates = decoder.push(&packet_at(&acquisition, 1_000));
        assert_eq!(updates.len(), 1, "the page still shows");
        assert_eq!(updates[0].regions.len(), 1, "the region survived");
        assert_eq!(decoder.held_bytes(), held);

        // A MODE CHANGE empties them.
        let mode_change = data_field(&[page(5, 2, &[]), end_of_display_set()]);
        decoder.push(&packet_at(&mode_change, 2_000));
        assert_eq!(
            decoder.held_bytes(),
            0,
            "a mode change must give the region buffers back"
        );
    }

    /// A packet at a NEW running time force-terminates the set that was open,
    /// even with no end-of-display-set segment in sight.
    #[test]
    fn a_new_running_time_force_terminates_the_open_set() {
        gst::init().unwrap();
        let mut decoder = taught();

        // A display set with NO end segment.
        let unterminated = data_field(&[
            page(5, 0, &[(1, 0, 0)]),
            region(1, 4, 2, 4, 1, 0, true, &[(5, 0, 0)]),
            clut(1, &[(1, 235, 128, 128, 0)]),
            object(5, &four_bit_string(&[(1, 4)]), &four_bit_string(&[(1, 4)])),
        ]);
        assert!(
            decoder.push(&packet_at(&unterminated, 1_000)).is_empty(),
            "nothing closed it yet"
        );

        // The next packet, at another time, is what closes it.
        let next = data_field(&[page(5, 0, &[(1, 0, 0)])]);
        let updates = decoder.push(&packet_at(&next, 2_000));
        assert_eq!(updates.len(), 1, "the open set was never terminated");
        assert_eq!(
            updates[0].start_rt,
            gst::ClockTime::from_mseconds(1_000),
            "the terminated set belongs to the time it was built for"
        );
    }

    /// A page with several regions on it is several regions on screen.
    #[test]
    fn a_multi_region_page_is_multiple_regions() {
        gst::init().unwrap();
        let mut decoder = taught();

        let string = four_bit_string(&[(1, 4)]);
        let bytes = data_field(&[
            page(5, 0, &[(1, 10, 20), (2, 300, 400)]),
            region(1, 4, 2, 4, 1, 0, true, &[(5, 0, 0)]),
            region(2, 4, 2, 4, 1, 0, true, &[(6, 0, 0)]),
            clut(1, &[(1, 235, 128, 128, 0)]),
            object(5, &string, &string),
            object(6, &string, &string),
            end_of_display_set(),
        ]);
        let updates = decoder.push(&packet_at(&bytes, 0));
        assert_eq!(updates[0].regions.len(), 2);
        let positions: Vec<_> = updates[0].regions.iter().map(|r| (r.x, r.y)).collect();
        assert!(positions.contains(&(10, 20)) && positions.contains(&(300, 400)));
    }

    /// An empty page is the clear: zero regions at its own time, which is how a
    /// broadcast takes a subtitle away before the timeout.
    #[test]
    fn an_empty_page_clears_the_screen() {
        gst::init().unwrap();
        let mut decoder = taught();
        assert_eq!(decoder.push(&packet_at(&minimal_display_set(), 0)).len(), 1);

        let updates = decoder.push(&packet_at(&empty_page(), 4_000));
        assert_eq!(updates.len(), 1);
        assert!(updates[0].regions.is_empty(), "the clear has no regions");
        assert_eq!(updates[0].start_rt, gst::ClockTime::from_mseconds(4_000));
    }

    /// Malformed input is a counted reset and never a panic.
    #[test]
    fn malformed_segments_are_counted_resets_and_never_panics() {
        gst::init().unwrap();

        // Not a subtitle data field at all.
        let mut decoder = taught();
        assert!(decoder.push(&packet_at(&[0x21, 0x00, 0x0F], 0)).is_empty());
        assert_eq!(decoder.take_decode_errors(), 1);

        // A segment claiming more bytes than the packet carries.
        let mut decoder = taught();
        let mut truncated = vec![0x20, 0x00, 0x0F, SEGMENT_PAGE, 0, 1];
        truncated.extend_from_slice(&0xFFFFu16.to_be_bytes());
        assert!(decoder.push(&packet_at(&truncated, 0)).is_empty());
        assert_eq!(decoder.take_decode_errors(), 1);

        // A character object at the end of a region segment, which advances the
        // walk by eight bytes rather than six: the fuzz target found that
        // `len - at` underflows there, which is a panic on a handful of bytes.
        let mut decoder = taught();
        let mut payload = vec![1u8, 1 << 3, 0, 8, 0, 4, 2 << 2, 1, 0, 0];
        payload.extend_from_slice(&[0, 5, 0x40, 0, 0, 0]); // a type-1 object
        let character = data_field(&[
            page(5, 0, &[(1, 0, 0)]),
            segment(0x11, &payload),
            end_of_display_set(),
        ]);
        decoder.push(&packet_at(&character, 0));
        assert!(decoder.take_decode_errors() <= 1, "it must not panic");

        // A region bigger than any display.
        let mut decoder = taught();
        let huge = data_field(&[
            page(5, 0, &[(1, 0, 0)]),
            region(1, 60_000, 60_000, 4, 1, 0, true, &[]),
            end_of_display_set(),
        ]);
        assert!(decoder.push(&packet_at(&huge, 0)).is_empty());
        assert_eq!(decoder.take_decode_errors(), 1);

        // Noise, at length.
        let mut decoder = taught();
        let mut noise = vec![0x20, 0x00];
        for index in 0..4096u32 {
            noise.push((index.wrapping_mul(2_654_435_761) >> 13) as u8);
        }
        decoder.push(&packet_at(&noise, 0));
        assert_eq!(
            decoder.push(&packet_at(&minimal_display_set(), 100)).len(),
            1,
            "the decoder never recovered from the noise"
        );
    }

    /// The allocation cap, at the site it was built for: PERSISTENT
    /// region buffers, which a stream can keep declaring.
    #[test]
    fn regions_past_the_budget_are_a_counted_reset() {
        gst::init().unwrap();
        let mut decoder = taught();

        // 4096x4096 indices is 16 MiB per region; two of them plus the RGBA
        // they expand into is far past 32 MiB.
        let mut segments = vec![page(5, 0, &[(1, 0, 0), (2, 0, 0)])];
        segments.push(region(1, 4096, 4096, 4, 1, 0, true, &[]));
        segments.push(region(2, 4096, 4096, 4, 1, 0, true, &[]));
        segments.push(end_of_display_set());
        let updates = decoder.push(&packet_at(&data_field(&segments), 0));

        assert!(
            updates.is_empty(),
            "two 16 MiB regions fitted a 32 MiB budget"
        );
        assert_eq!(decoder.budget_resets(), 1);
        assert_eq!(decoder.take_decode_errors(), 1);
        assert!(
            decoder.held_bytes() <= ALLOCATION_BUDGET,
            "the decoder is holding {} bytes",
            decoder.held_bytes()
        );
        assert!(decoder.allocated_bytes() <= decoder.held_bytes());

        // AND THE REGION-SIDE CHECK ON ITS OWN, with NO end-of-display-set to
        // let the display set's own pricing catch the same overrun a moment
        // later. What it observes is what the decoder is HOLDING, which is the
        // cap's actual subject.
        let mut decoder = taught();
        // THREE, not two: 4096x4096 indices is exactly half the budget, so two
        // of them fit it to the byte and only the third is over. (Finding that
        // out is what the bite proof is for: the first version of this test
        // asserted a breach that could not happen.)
        let open = data_field(&[
            page(5, 0, &[(1, 0, 0), (2, 0, 0), (3, 0, 0)]),
            region(1, 4096, 4096, 4, 1, 0, true, &[]),
            region(2, 4096, 4096, 4, 1, 0, true, &[]),
            region(3, 4096, 4096, 4, 1, 0, true, &[]),
        ]);
        decoder.push(&packet_at(&open, 0));
        assert!(
            decoder.held_bytes() <= ALLOCATION_BUDGET,
            "the decoder is holding {} bytes against a budget of {ALLOCATION_BUDGET}",
            decoder.held_bytes()
        );
        assert_eq!(
            decoder.budget_resets(),
            1,
            "the region's own charge never fired"
        );

        // Recovery: the cap resets the decoder, it does not disable it.
        let mut decoder = taught();
        assert_eq!(
            decoder.push(&packet_at(&minimal_display_set(), 100)).len(),
            1
        );
    }

    /// A blank screen this decoder is responsible for is counted; a blank
    /// screen the stream asked for is not.
    ///
    /// After a budget fault the decoder returns
    /// `Ok(None)` for every display set until the stream rebuilds its state,
    /// and nothing anywhere said so. A viewer looking at nothing and an
    /// operator reading the counters could not tell the difference between
    /// a broadcast with no subtitle on screen and a decoder that had given
    /// up.
    #[test]
    fn a_blank_the_decoder_is_responsible_for_is_counted_and_a_clear_is_not() {
        gst::init().unwrap();
        let mut decoder = taught();

        // A CLEAR the stream asked for: an empty page composition. Reported as
        // an update with no regions, and not counted.
        decoder.push(&packet_at(&minimal_display_set(), 0));
        let updates = decoder.push(&packet_at(
            &data_field(&[page(5, 0, &[]), end_of_display_set()]),
            1_000,
        ));
        assert_eq!(updates.len(), 1, "the clear did not reach the engine");
        assert!(updates[0].regions.is_empty());
        assert_eq!(
            decoder.suppressed_sets(),
            0,
            "a clear the stream asked for was counted against the decoder"
        );

        // A BLANK the decoder is responsible for: a budget fault takes the
        // state, and the display sets that follow have nothing to draw into.
        let mut decoder = taught();
        let over_budget = data_field(&[
            page(5, 0, &[(1, 0, 0), (2, 0, 0)]),
            region(1, 4096, 4096, 4, 1, 0, true, &[]),
            region(2, 4096, 4096, 4, 1, 0, true, &[]),
            end_of_display_set(),
        ]);
        assert!(decoder.push(&packet_at(&over_budget, 0)).is_empty());
        assert_eq!(decoder.budget_resets(), 1);
        assert_eq!(decoder.take_decode_errors(), 1);
        assert_eq!(
            decoder.suppressed_sets(),
            0,
            "the set that FAILED is a decode error, not a suppressed one"
        );

        // The stream carries on. Its next sets paint into regions that are gone,
        // so they are blank, and each one is counted.
        let object_only = data_field(&[
            object(5, &four_bit_string(&[(1, 8)]), &four_bit_string(&[(1, 8)])),
            end_of_display_set(),
        ]);
        for (n, rt) in [1_000u64, 2_000].into_iter().enumerate() {
            assert!(decoder.push(&packet_at(&object_only, rt)).is_empty());
            assert_eq!(
                decoder.suppressed_sets(),
                n as u64 + 1,
                "a blank display set went uncounted"
            );
            assert_eq!(
                decoder.take_decode_errors(),
                0,
                "a blank set was reported as a decode error as well"
            );
        }

        // AND RECOVERY IS STREAM-GATED, which is the contract: the counter stops
        // the moment the stream rebuilds the state, and nothing this decoder
        // could do would make it stop sooner.
        assert_eq!(
            decoder
                .push(&packet_at(&minimal_display_set(), 3_000))
                .len(),
            1
        );
        assert_eq!(
            decoder.suppressed_sets(),
            2,
            "a set that drew something was counted as blank"
        );
    }

    /// A stream that declares regions nobody displays does not starve the one
    /// that is displayed.
    ///
    /// Found by the fuzz target on its second campaign, through the recovery
    /// invariant rather than a crash: after enough undisplayed regions the
    /// budget was full and every later display set was refused, the same
    /// starvation the PGS object store's own rule prevents, in the one
    /// format whose buffers are SUPPOSED to persist. Regions the page does not
    /// name and this display set has not composed are given up first, and only
    /// then is a region refused.
    #[test]
    fn regions_nobody_displays_are_spilled_before_one_that_is() {
        gst::init().unwrap();
        let mut decoder = taught();

        // Fill most of the budget with regions no page ever names: eight of
        // 2048x2048 is 32 MiB of indices.
        for id in 10..18u8 {
            let bytes = data_field(&[
                page(5, 0, &[]),
                region(id, 2048, 2048, 4, 1, 0, true, &[]),
                end_of_display_set(),
            ]);
            decoder.push(&packet_at(&bytes, u64::from(id) * 100));
        }
        assert!(
            decoder.held_bytes() <= ALLOCATION_BUDGET,
            "the cap did not hold while the regions were being declared"
        );

        // Now a display set that actually shows something must still work.
        let updates = decoder.push(&packet_at(&minimal_display_set(), 5_000));
        assert_eq!(
            updates.len(),
            1,
            "a display set with a picture in it was starved by regions with none"
        );
        assert_eq!(updates[0].regions.len(), 1);
        assert_eq!(decoder.budget_resets(), 0, "spilling is not a breach");
        assert!(decoder.allocated_bytes() <= decoder.held_bytes());
    }

    /// The accounting never under-reports the memory, through the paint cycle
    /// this format is built around.
    #[test]
    fn the_accounting_follows_the_persistent_regions() {
        gst::init().unwrap();
        let mut decoder = taught();
        assert_eq!(decoder.held_bytes(), 0);

        let bytes = data_field(&[
            page(5, 0, &[(1, 0, 0)]),
            region(1, 100, 50, 4, 1, 0, true, &[(5, 0, 0)]),
            clut(1, &[(1, 235, 128, 128, 0)]),
            object(
                5,
                &four_bit_string(&[(1, 100)]),
                &four_bit_string(&[(1, 100)]),
            ),
            end_of_display_set(),
        ]);
        decoder.push(&packet_at(&bytes, 0));
        assert!(
            decoder.held_bytes() >= 100 * 50,
            "the region's own buffer is not being charged"
        );
        assert!(decoder.allocated_bytes() <= decoder.held_bytes());

        // A resize gives the old buffer back and charges the new one.
        let resized = data_field(&[
            page(5, 0, &[(1, 0, 0)]),
            region(1, 10, 5, 4, 1, 0, true, &[]),
            end_of_display_set(),
        ]);
        decoder.push(&packet_at(&resized, 1_000));
        assert!(
            decoder.held_bytes() < 100 * 50,
            "a shrunk region is still charged its old size"
        );
        assert!(decoder.allocated_bytes() <= decoder.held_bytes());
    }

    /// A force-terminate that FAILS does not swallow the packet that
    /// triggered it.
    ///
    /// The set being terminated is the old one; the packet in hand is a new
    /// display set at a new time and has nothing to do with it. The fuzz target
    /// found a stream that reached a state where the terminated set always
    /// failed, and every packet after it was thrown away on the way in, the
    /// commit that fixed it claimed a seed this vector is the durable half of.
    #[test]
    fn a_failing_force_terminate_does_not_swallow_the_next_packet() {
        gst::init().unwrap();
        let mut decoder = taught();

        // An OPEN display set whose CLOSE is guaranteed to breach while the
        // regions themselves fit: two regions of 8192x2000 are 32.8 MB of
        // indices, inside the budget, and the RGBA they expand into is four
        // times that. No end-of-display-set segment, so it stays open.
        let open = data_field(&[
            page(5, 0, &[(1, 0, 0), (2, 0, 0)]),
            region(1, 8192, 2000, 4, 1, 0, true, &[]),
            region(2, 8192, 2000, 4, 1, 0, true, &[]),
        ]);
        assert!(decoder.push(&packet_at(&open, 0)).is_empty());
        assert_eq!(
            decoder.budget_resets(),
            0,
            "the two regions were supposed to fit; only their expansion is over"
        );

        // A new display set at a new time: the force-terminate runs, fails, and
        // must not take this one with it.
        let updates = decoder.push(&packet_at(&minimal_display_set(), 1_000));
        assert_eq!(
            updates.len(),
            1,
            "the packet that triggered the force-terminate was swallowed by its failure"
        );
        assert_eq!(updates[0].regions.len(), 1);
    }

    /// A re-shown page hands back the SAME pixels, and a repainted one does
    /// not.
    ///
    /// The engine's pending store accounts by allocation and dedupes shared
    /// pixels by pointer, on the stated grounds that a DVB page re-emitted
    /// without changing is N pointers to one allocation, and its no-op
    /// adoption check compares by pointer too, so a redelivered page should not
    /// cost a repaint. Both were false until the expansion was cached: every
    /// set allocated a fresh one.
    ///
    /// The second half is the one that matters more: a cache that is not
    /// invalidated shows the viewer a stale subtitle.
    #[test]
    fn a_reshown_page_reuses_its_pixels_and_a_repainted_one_does_not() {
        gst::init().unwrap();
        let mut decoder = taught();

        let string = four_bit_string(&[(1, 8)]);
        let first = data_field(&[
            page(5, 0, &[(1, 0, 0)]),
            region(1, 8, 2, 4, 1, 0, true, &[(5, 0, 0)]),
            clut(1, &[(1, 235, 128, 128, 0), (2, 81, 240, 90, 0)]),
            // A second palette, defined now and used much later: the arm that
            // re-describes the region against it must not define it in the same
            // display set, or a CLUT definition's own invalidation would stand
            // in for the composition's.
            clut(2, &[(1, 81, 240, 90, 0), (2, 235, 128, 128, 0)]),
            object(5, &string, &string),
            end_of_display_set(),
        ]);
        let first_pixels = decoder.push(&packet_at(&first, 0))[0].regions[0]
            .pixels
            .clone();

        // A set that re-shows the page without touching it: same pointer.
        let reshow = data_field(&[page(5, 0, &[(1, 0, 0)]), end_of_display_set()]);
        let updates = decoder.push(&packet_at(&reshow, 1_000));
        assert!(
            Arc::ptr_eq(&updates[0].regions[0].pixels, &first_pixels),
            "a re-shown page expanded itself again"
        );

        // A set that PAINTS into it: a new expansion, and the pixels differ.
        let repaint = data_field(&[
            page(5, 0, &[(1, 0, 0)]),
            region(1, 8, 2, 4, 1, 0, false, &[(6, 0, 0)]),
            object(6, &four_bit_string(&[(2, 4)]), &four_bit_string(&[(2, 4)])),
            end_of_display_set(),
        ]);
        let updates = decoder.push(&packet_at(&repaint, 2_000));
        let painted = &updates[0].regions[0].pixels;
        assert!(
            !Arc::ptr_eq(painted, &first_pixels),
            "a repainted page handed back its old pixels: the cache is not invalidated"
        );
        assert_ne!(
            painted[0..4],
            first_pixels[0..4],
            "the repaint did not reach the pixels"
        );

        // A PAINT WITH NO COMPOSITION: an object segment on its own, drawn
        // through the placement the region already holds. Nothing about the
        // region's description changed, so only the paint itself can know the
        // expansion is stale.
        let before = updates[0].regions[0].pixels.clone();
        let paint_only = data_field(&[
            object(6, &four_bit_string(&[(1, 4)]), &four_bit_string(&[(1, 4)])),
            end_of_display_set(),
        ]);
        let updates = decoder.push(&packet_at(&paint_only, 2_500));
        assert!(
            !Arc::ptr_eq(&updates[0].regions[0].pixels, &before),
            "a paint left the old pixels cached"
        );

        // A COMPOSITION WITH NO PAINT: the region is re-described against a
        // DIFFERENT palette. Not one pixel index changes, so only the
        // composition can know the colours are stale.
        let before = updates[0].regions[0].pixels.clone();
        let recompose = data_field(&[
            page(5, 0, &[(1, 0, 0)]),
            region(1, 8, 2, 4, 2, 0, false, &[(6, 0, 0)]),
            end_of_display_set(),
        ]);
        let updates = decoder.push(&packet_at(&recompose, 2_800));
        assert!(
            !Arc::ptr_eq(&updates[0].regions[0].pixels, &before),
            "a region re-described against another palette kept its old colours"
        );

        // And a PALETTE change invalidates too, without touching a pixel index.
        let recolour = data_field(&[
            page(5, 0, &[(1, 0, 0)]),
            clut(1, &[(1, 81, 240, 90, 0), (2, 81, 240, 90, 0)]),
            end_of_display_set(),
        ]);
        let before = updates[0].regions[0].pixels.clone();
        let updates = decoder.push(&packet_at(&recolour, 3_000));
        assert!(
            !Arc::ptr_eq(&updates[0].regions[0].pixels, &before),
            "a palette definition left the old colours cached"
        );
    }

    /// A page may name one region at several positions, and drawing it twice
    /// costs one expansion, not two.
    ///
    /// Found by the fuzz target, which built a page naming the same region six
    /// times and watched the display-set budget refuse it: the pricing was per
    /// PAGE ENTRY while the allocation is per REGION. Both are per region now,
    /// and the pixels are shared by reference count, which is the sharing the
    /// engine's own pending-store budget expects from this format.
    #[test]
    fn a_region_named_twice_on_a_page_is_expanded_once() {
        gst::init().unwrap();
        let mut decoder = taught();

        let string = four_bit_string(&[(1, 4)]);
        let bytes = data_field(&[
            page(5, 0, &[(1, 10, 20), (1, 300, 400)]),
            region(1, 4, 2, 4, 1, 0, true, &[(5, 0, 0)]),
            clut(1, &[(1, 235, 128, 128, 0)]),
            object(5, &string, &string),
            end_of_display_set(),
        ]);
        let updates = decoder.push(&packet_at(&bytes, 0));
        assert_eq!(updates[0].regions.len(), 2, "both positions are drawn");
        assert!(
            Arc::ptr_eq(&updates[0].regions[0].pixels, &updates[0].regions[1].pixels),
            "the same region was expanded twice for one page"
        );
    }

    /// A region segment can name ten thousand object placements, and every
    /// one of them is memory this decoder keeps.
    ///
    /// Measured: 48 MiB of real heap against a
    /// `held_bytes()` of 256: the placements were not counted at all, and
    /// `clear()` handed their capacity forward for free. The budget is only
    /// worth what it counts.
    #[test]
    fn object_placements_are_counted_and_their_capacity_is_shed() {
        gst::init().unwrap();
        let mut decoder = taught();

        // 10 920 placements is the most one 65 535-byte region segment can
        // carry: ten bytes of header and six per entry.
        let objects: Vec<(u16, u16, u16)> =
            (0..10_920u32).map(|index| (index as u16, 0, 0)).collect();
        let bytes = data_field(&[region(1, 1, 1, 4, 0, 0, true, &objects)]);
        decoder.push(&packet_at(&bytes, 0));

        let held = decoder.held_bytes();
        assert!(
            held >= (objects.len() * std::mem::size_of::<Placement>()) as u64,
            "ten thousand placements are being held and charged {held} bytes"
        );
        assert!(decoder.allocated_bytes() <= held);

        // A composition with no objects gives the capacity back rather than
        // keeping room for ten thousand more.
        let empty = data_field(&[region(1, 1, 1, 4, 0, 0, true, &[])]);
        decoder.push(&packet_at(&empty, 0));
        assert!(
            decoder.held_bytes() < held / 100,
            "the placement list kept its capacity: {} against {held}",
            decoder.held_bytes()
        );

        // AND THE SEGMENT'S OWN LIST IS PRICED BEFORE IT IS STORED. With the
        // budget nearly full of pixels, a region composition whose picture is
        // one pixel and whose placement list is ten thousand entries has to be
        // refused for the list, the only thing it is really asking for.
        let mut decoder = taught();
        let big = data_field(&[region(1, 8192, 4020, 4, 0, 0, true, &[])]);
        decoder.push(&packet_at(&big, 0));
        assert_eq!(decoder.budget_resets(), 0, "the first region should fit");
        let list = data_field(&[region(2, 1, 1, 4, 0, 0, true, &objects)]);
        decoder.push(&packet_at(&list, 0));
        assert_eq!(
            decoder.budget_resets(),
            1,
            "a segment asking for ten thousand placements was not priced for them"
        );

        // AND THE CAP HOLDS while a stream tries to fill it this way: 256
        // regions of ten thousand placements each is 48 MiB of real heap.
        let mut decoder = taught();
        for id in 0..=255u8 {
            let bytes = data_field(&[region(id, 1, 1, 4, 0, 0, true, &objects)]);
            decoder.push(&packet_at(&bytes, 0));
            assert!(
                decoder.held_bytes() <= ALLOCATION_BUDGET,
                "region {id}: holding {} bytes",
                decoder.held_bytes()
            );
            assert!(decoder.allocated_bytes() <= decoder.held_bytes());
        }
    }

    /// A mode change does not eat the display set it arrives in.
    ///
    /// The page composition carries the timeout and the mode-change flag in the
    /// same two bytes, and the display definition arrives in the same set. Both
    /// were being destroyed by the reset the flag triggers: an acquisition set
    /// asking for five seconds got one, and one carrying a 1920x1080 grid
    /// reverted to 720x576 and put its regions off the picture, dropping the
    /// whole set silently.
    ///
    /// The earlier vectors could not see it because they used the DEFAULT grid
    /// and a timeout the clamp happened to produce.
    #[test]
    fn a_mode_change_keeps_the_display_set_it_arrives_in() {
        gst::init().unwrap();
        let mut decoder = DvbDecoder::new();
        decoder.set_video_size(1920, 1080);

        // Something for the mode change to actually clear.
        decoder.push(&packet_at(&minimal_display_set(), 0));
        assert!(decoder.held_bytes() > 0);

        let string = four_bit_string(&[(1, 8)]);
        let acquisition = data_field(&[
            display_definition(1920, 1080, None),
            page(5, 2, &[(1, 100, 900)]),
            region(1, 8, 4, 4, 1, 0, true, &[(5, 0, 0)]),
            clut(1, &[(1, 235, 128, 128, 0)]),
            object(5, &string, &string),
            end_of_display_set(),
        ]);
        let updates = decoder.push(&packet_at(&acquisition, 1_000));

        assert_eq!(updates.len(), 1, "the acquisition set was dropped");
        assert_eq!(
            updates[0].end_rt,
            Some(gst::ClockTime::from_mseconds(6_000)),
            "the mode change ate the page's own five-second timeout"
        );
        let region = &updates[0].regions[0];
        assert_eq!(
            (region.x, region.y),
            (100, 900),
            "the mode change ate the display definition that arrived with it, so the region \
             was placed on the default grid and stretched off the picture"
        );
    }

    /// THE RENDER PROOF: a hand-built display set through the ENGINE's
    /// production wiring, and out as a source-frame overlay whose own timeout
    /// takes it away again.
    #[test]
    fn a_display_set_reaches_the_overlay_set_and_times_out() {
        gst::init().unwrap();
        let engine = CueEngine::new();
        engine.set_video_size(720, 576);

        engine.submit_bitmap(packet_at(&minimal_display_set(), 1_000));
        let at = gst::ClockTime::from_mseconds(1_200);
        assert!(
            wait_for(|| !engine.overlays_for(Some(at)).is_empty()),
            "the decoded display set never reached the overlay set"
        );
        let overlays = engine.overlays_for(Some(at));
        assert_eq!(overlays.len(), 1);
        assert_eq!(overlays[0].space, OverlaySpace::SrcFrame);
        assert_eq!((overlays[0].x, overlays[0].y), (100, 200));
        assert_eq!(engine.bitmap_decode_errors(), 0);
        assert_eq!(engine.bitmap_overflow_resets(), 0);
        assert_eq!(engine.bitmap_dropped_sets(), 0);

        // THE TIMEOUT, from the page's own five seconds, with nothing behind it
        // to supersede it. This is the engine's expiry path fed by a real
        // decoder rather than a test one.
        let after = gst::ClockTime::from_mseconds(6_500);
        assert!(
            engine.overlays_for(Some(after)).is_empty(),
            "the page's timeout never took the subtitle off the screen"
        );
    }
}
