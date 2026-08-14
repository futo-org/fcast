//! Blu-ray Presentation Graphic Stream subtitles (`subpicture/x-pgs`).
//!
//! PGS is the Blu-ray subtitle format: a stream of SEGMENTS carrying a palette,
//! one or more run-length-encoded pictures ("objects") and a composition that
//! says where on the video canvas each picture goes. A run of segments ending
//! in an END segment is a DISPLAY SET, and a display set is what reaches the
//! screen.
//!
//! # Structure, as this decoder reads it
//!
//! Every segment is `[type u8][length u16be][payload]`, and the segment types
//! this format defines are:
//!
//! | type | segment | what this decoder takes from it |
//! |---|---|---|
//! | 0x14 | palette (PDS) | 5-byte `index, Y, Cr, Cb, A` entries, expanded to RGBA once |
//! | 0x15 | object (ODS) | the RLE picture, possibly FRAGMENTED across segments |
//! | 0x16 | presentation (PCS) | canvas size + where each object is composited |
//! | 0x17 | window (WDS) | validated and dropped, see below |
//! | 0x18 | interactive (ICS) | menus; skipped |
//! | 0x80 | end of display set | closes the set and emits a [`DisplayUpdate`] |
//!
//! A buffer is NOT guaranteed to hold whole segments, let alone a whole display
//! set: PGS in matroska usually delivers one set per buffer, but the format
//! permits a segment to straddle the boundary and transport-stream sources do
//! split them. So bytes accumulate in `carry` until a segment is complete, and
//! segments accumulate into an open set until the END arrives. Both are dropped
//! whole on [`SubpicDecoder::reset`]: a flush or a track change means the
//! half-assembled set describes a timeline that no longer exists.
//!
//! # Timing
//!
//! A display set is shown from the running time of the packet its PRESENTATION
//! segment arrived in, and shown until something replaces it: `end_rt` is
//! always `None`. That is not laziness, it is the format: PGS has no duration
//! field, and a subtitle is taken off the screen by a display set with NO
//! composition objects, which this decoder emits as a zero-region
//! [`DisplayUpdate`] (the engine's scheduled clear). A buffer duration, where a
//! container supplies one, is deliberately ignored for the same reason the
//! reference implementation ignores it: the stream's own clear is authoritative
//! and arrives on time.
//!
//! # Geometry
//!
//! Objects are composited onto the canvas the presentation segment declares
//! (the authoring video size, e.g. 1920x1080). This decoder maps that canvas
//! onto the coded video size with a FIT + CENTRE, keeping aspect: subtitles
//! authored for 1920x1080 shown on a 1280x720 picture land where the author put
//! them, and a canvas whose aspect differs from the video's is letterboxed
//! rather than stretched, because a stretched subtitle sits visibly off the
//! feature it annotates. Rects come out in VIDEO pixels; the texture stays at
//! its native size.
//!
//! # Provenance
//!
//! Written fresh against the format's structure. GStreamer's `gstspu-pgs.c`
//! (LGPL) was consulted as a NORMATIVE REFERENCE for behaviour the structure
//! does not fix, and each such consultation is named at the site that made the
//! decision: the YCbCr coefficients, the four RLE cases, ignoring the
//! composition state, keeping one palette, and tolerating a ragged palette
//! segment. Nothing here is a transliteration of it. The state machine, the
//! error policy, the geometry and the output model are all different, starting
//! with the one thing that had to be different: the reference PREMULTIPLIES its
//! palette by alpha (`gstspu-pgs.c:545-548`) and this decoder must not, because
//! the renderer composites straight alpha.
//!
//! # Hostile input
//!
//! Stream bytes are untrusted. Nothing here panics, asserts or allocates on a
//! promise it has not checked: every malformed structure is a counted, logged
//! RESET (the decoder drops back to "waiting for a presentation segment" and
//! the next display set recovers), and every allocation the decoder KEEPS or is
//! about to expand into is charged against a 32 MiB per-decoder budget whose
//! breach is the same counted reset: the object store, the carried bytes of a
//! half-read segment, and a display set's RGBA pages, the last of them priced
//! from their headers before the first byte is allocated.
//!
//! One allocation is NOT priced, and it is named here rather than left for a
//! reader to find: `consume` copies the carry and the incoming packet into one
//! scratch buffer to parse from. It is transient
//! (freed before `push` returns), it is not charged, and it is bounded by the
//! packet the driver was handed plus one segment, i.e. by the demuxer, not by
//! anything a display set claims about itself. That is the one place where a
//! 16 MiB buffer produces a 16 MiB allocation, and the only reason it is
//! acceptable is that the buffer already exists: the driver is holding it.

use std::{collections::HashMap, sync::Arc};

use tracing::{debug, warn};

use super::{BitmapPacket, BitmapRegion, DisplayUpdate, SubpicDecoder};

/// Segment types, from the format's own table. Public so that the fixtures and
/// the structure-aware fuzz target can name them rather than repeat them.
pub const SEGMENT_PALETTE: u8 = 0x14;
pub const SEGMENT_OBJECT: u8 = 0x15;
pub const SEGMENT_PRESENTATION: u8 = 0x16;
pub const SEGMENT_WINDOW: u8 = 0x17;
pub const SEGMENT_INTERACTIVE: u8 = 0x18;
pub const SEGMENT_END: u8 = 0x80;

/// `[type u8][length u16be]`.
const SEGMENT_HEADER: usize = 3;

/// A composition object carries a cropping rectangle after its fixed fields.
const COMPOSITION_FLAG_CROPPED: u8 = 0x80;
/// A composition object is part of a forced (burned-in) subtitle.
const COMPOSITION_FLAG_FORCED: u8 = 0x40;

/// This object segment BEGINS an object's RLE data and carries its total
/// length; without it the payload is a continuation to append.
const OBJECT_FLAG_FIRST_FRAGMENT: u8 = 0x80;

/// `index, Y, Cr, Cb, A`.
const PALETTE_ENTRY: usize = 5;

/// The per-decoder allocation budget: 32 MiB, four full-HD RGBA
/// pages.
///
/// Public because it is a CONTRACT, not an implementation detail: the fuzz
/// target asserts against it after every packet
/// ([`PgsDecoder::held_bytes`]), which is the only way "the cap holds" is a
/// claim rather than an intention.
///
/// Charged against everything this decoder holds or is about to allocate: the
/// object store's declared RLE lengths and the RGBA pages a display set is
/// about to expand into. A breach is a counted reset, never a panic and never a
/// partial render: a stream that asks for more than this is either corrupt or
/// hostile, and in both cases the right answer is to forget what it said and
/// wait for the next display set.
pub const ALLOCATION_BUDGET: u64 = 32 * 1024 * 1024;

/// The most [`PgsDecoder::carry`] can hold: one segment, header included.
///
/// RESERVED out of the budget rather than checked against it, because the carry
/// is filled by the framing, after the object store has already been charged,
/// so a check at the object's allocation site cannot see it coming. Found by
/// the fuzz target on its first campaign: an object store talked up to exactly
/// the budget, then a packet ending mid-segment, and the decoder held 22 bytes
/// more than it was allowed to.
const CARRY_RESERVE: u64 = (SEGMENT_HEADER + 0xFFFF) as u64;

/// What everything except the carry may hold, so that
/// `held_bytes() <= ALLOCATION_BUDGET` is exact.
const WORKING_BUDGET: u64 = ALLOCATION_BUDGET - CARRY_RESERVE;

/// Straight-alpha RGBA8, one palette entry.
///
/// Shared with the other decoders (`subpic::vobsub`): the renderer's pixel
/// format is one decision, not one per format.
pub type Rgba = [u8; 4];

/// Why a display set is being thrown away. Both outcomes are the same reset;
/// they are distinguished because "this stream is corrupt" and "this stream is
/// asking for more memory than it may have" are different things to see in a
/// log, and because the budget's own test has to be able to tell them apart.
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

/// A cropping rectangle inside an object's picture. Only the part it names is
/// composited, and it lands at the composition object's position.
#[derive(Debug, Clone, Copy)]
struct Crop {
    x: u16,
    y: u16,
    width: u16,
    height: u16,
}

/// One entry of a presentation segment's composition: which object goes where.
#[derive(Debug, Clone, Copy)]
struct CompositionObject {
    object_id: u16,
    /// Which window the object is composited into. Parsed and carried for the
    /// log line only (see [`window_segment`]).
    window_id: u8,
    /// The format's "forced" bit: a subtitle the disc wants shown even with
    /// subtitles switched off (signs, foreign dialogue). Parsed and carried no
    /// further, since surfacing it is a receiver UX decision of its own. It is
    /// parsed so the flag byte is fully accounted for and a future forced-only
    /// mode has the bit already in hand.
    forced: bool,
    x: u16,
    y: u16,
    crop: Option<Crop>,
}

/// An object's RLE data, complete when `data.len() == declared`.
struct ObjectData {
    /// The version the first fragment carried; a continuation naming a
    /// different one belongs to an object being replaced and is dropped.
    version: u8,
    /// The 24-bit total the first fragment declared.
    declared: usize,
    data: Vec<u8>,
}

impl ObjectData {
    /// What this object is charged against the budget.
    ///
    /// The real ALLOCATION, and the `declared` length it has committed to reach
    /// (whichever is larger). Not `declared` alone, and not `len`:
    /// the buffer is allocated to `declared` up front precisely
    /// so that these three numbers agree, but an allocator that rounds up, or a
    /// future path that grows the buffer, must be charged for what it actually
    /// took rather than for what it meant to take. The accounting is what the
    /// cap is made of, and the DVB decoder inherits it.
    fn charge(&self) -> u64 {
        self.data.capacity().max(self.declared) as u64
    }
}

/// A display set being assembled: the presentation segment has arrived, the END
/// segment has not.
struct OpenSet {
    /// Running time of the packet the PRESENTATION segment arrived in, i.e.
    /// when this set goes on screen.
    rt: gst::ClockTime,
    /// The authoring canvas, from the presentation segment's video descriptor.
    canvas: (u16, u16),
    objects: Vec<CompositionObject>,
}

/// The PGS decoder: packets in, display updates out.
pub struct PgsDecoder {
    /// Bytes of an incomplete segment, kept for the next packet. Bounded by one
    /// segment (`3 + 0xFFFF`) because whole segments are always consumed.
    carry: Vec<u8>,
    /// The running time the carried bytes belong to. Every packet of one
    /// display set shares a presentation time. That is true of PGS in matroska
    /// (one set, one block) and of PGS in a transport stream (one set, one PTS)
    /// so a packet at a NEW running time cannot be the rest of a segment left
    /// half-read at the old one.
    ///
    /// This is what keeps the carry from becoming a wedge. Without it, one
    /// corrupt length field misframes the byte stream and every later display
    /// set is swallowed as the payload of a segment that will never end:
    /// subtitles silently off for the rest of the file, with nothing in the log
    /// but the first complaint. Measured, on a fuzz-shaped vector, before it
    /// was added.
    carry_rt: Option<gst::ClockTime>,
    /// The current palette, already RGBA with STRAIGHT alpha.
    palette: [Rgba; 256],
    /// Decoded-object store, keyed by object id and kept ACROSS display sets:
    /// a palette-only update names an object it does not re-send.
    objects: HashMap<u16, ObjectData>,
    open: Option<OpenSet>,
    /// The coded video size, once the engine has taught it. Until then the
    /// canvas is used as its own target, which is the identity mapping.
    video_size: Option<(u32, u32)>,
    /// Counted resets not yet handed to the engine.
    errors: u64,
    /// The subset of resets that were allocation-budget breaches, cumulative.
    /// Not drained: it is a property of the STREAM, and a test that wants to
    /// prove the cap fired needs to distinguish it from an ordinary parse
    /// failure.
    budget_resets: u64,
    /// Whether the last thing that happened was a failure, so a stream of
    /// garbage logs one warning rather than one per packet.
    failing: bool,
}

impl Default for PgsDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl PgsDecoder {
    pub fn new() -> Self {
        Self {
            carry: Vec::new(),
            carry_rt: None,
            palette: [[0; 4]; 256],
            objects: HashMap::new(),
            open: None,
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
    /// The object store's allocations plus the buffer carrying a half-delivered
    /// segment. Both counted by CAPACITY rather than by length, because
    /// capacity is what the allocator is holding and length is only what the
    /// decoder has put in it. The distinction is not pedantry: both leaks found
    /// here were invisible to a length-based count.
    ///
    /// Everything else a display set costs is transient: a decoded picture is
    /// either handed to the engine (where the pending store's own budget takes
    /// over) or dropped before this returns. What that peak costs is priced in
    /// [`Self::close_set`] before any of it is allocated.
    ///
    /// Exposed for the fuzz target, which asserts the cap after every packet,
    /// the one invariant that cannot be proved by any single hand-written
    /// vector, because it is a claim about ALL inputs.
    pub fn held_bytes(&self) -> u64 {
        self.object_bytes() + self.carry.capacity() as u64
    }

    /// What this decoder's buffers have actually taken from the allocator.
    ///
    /// The independent half of the cap's contract: [`Self::held_bytes`] is what
    /// the decoder BELIEVES it holds and this is what it holds, and the
    /// invariant that matters is that the belief is never smaller than the
    /// truth. Both of the ways it can be (a `Vec` whose capacity outlived its
    /// bytes, and an object buffer that doubled
    /// behind a charge based on the length it had promised), and neither was
    /// visible to any number the decoder was reporting at the time, so finding
    /// them needed a counting allocator. This is that counter, at the two
    /// buffers a decoder actually keeps, so the property is a test and a fuzz
    /// invariant rather than an audit.
    #[doc(hidden)]
    pub fn allocated_bytes(&self) -> u64 {
        self.objects
            .values()
            .map(|object| object.data.capacity() as u64)
            .sum::<u64>()
            + self.carry.capacity() as u64
    }

    /// Drop everything the stream taught, keep everything the ENGINE taught.
    ///
    /// The difference matters and is the reason this is not
    /// [`SubpicDecoder::reset`]: a malformed segment invalidates the decoder's
    /// idea of the stream, not the video size it was told out of band, and the
    /// engine only re-teaches that after a reset it initiated (the F1
    /// contract). A parse failure that quietly forgot the video size would
    /// scale every later set onto the wrong grid.
    fn fail(&mut self, fault: Fault) {
        self.errors += 1;
        if matches!(fault, Fault::Budget(_)) {
            self.budget_resets += 1;
        }
        let reason = fault.reason();
        if self.failing {
            debug!(reason, "pgs: dropping the display set");
        } else {
            warn!(
                reason,
                "pgs: dropping the display set and waiting for the next presentation segment; \
                 this line is not repeated until a set decodes"
            );
            self.failing = true;
        }
        // `Vec::new()`, not `clear()`: a `clear` keeps the CAPACITY, and the
        // capacity is the allocation. One 16 MiB packet ending mid-segment
        // leaves a 16 MiB buffer behind a carry of four bytes, and a `clear`
        // hands that buffer forward for the life of the decoder, measured, and
        // it survived `reset` too.
        self.carry = Vec::new();
        self.carry_rt = None;
        self.open = None;
        self.objects = HashMap::new();
        self.palette = [[0; 4]; 256];
    }

    /// What the object store currently costs.
    fn object_bytes(&self) -> u64 {
        self.objects.values().map(ObjectData::charge).sum()
    }

    /// Parse as many whole segments as `bytes` (behind whatever was carried
    /// over) contains, and answer with every display set they closed.
    fn consume(&mut self, bytes: &[u8], rt: gst::ClockTime) -> Vec<DisplayUpdate> {
        if !self.carry.is_empty() && self.carry_rt != Some(rt) {
            // The display set that owned those bytes has been superseded by one
            // that starts at another time, so the segment they belong to will
            // never be finished. Counted: a truncated set IS a malformed
            // stream, and this is the only place that can tell.
            self.fail(Fault::Malformed(
                "a segment left unfinished by the display set before it",
            ));
        }
        // One copy per packet, on the decode worker and never on a streaming
        // thread. A PGS packet is a display set (kilobytes) so the copy is
        // not worth the two parse paths that avoiding it would need.
        let mut buffer = std::mem::take(&mut self.carry);
        buffer.extend_from_slice(bytes);

        let mut updates = Vec::new();
        let mut at = 0usize;
        while at + SEGMENT_HEADER <= buffer.len() {
            let kind = buffer[at];
            let length = u16::from_be_bytes([buffer[at + 1], buffer[at + 2]]) as usize;
            let payload = at + SEGMENT_HEADER;
            if payload + length > buffer.len() {
                // INCOMPLETE, not malformed: the rest of this segment is in a
                // packet that has not arrived.
                break;
            }
            match self.segment(kind, &buffer[payload..payload + length], rt) {
                Ok(Some(update)) => updates.push(update),
                Ok(None) => {}
                Err(fault) => {
                    // The rest of this packet belongs to the set that just went
                    // wrong; `fail` drops the carry with it, and the next
                    // presentation segment re-syncs.
                    self.fail(fault);
                    return updates;
                }
            }
            at = payload + length;
        }
        // REBUILT from the tail, never drained-and-kept. `drain` leaves the
        // buffer's capacity where the largest packet ever seen put it, and that
        // capacity is charged to nobody and freed by nothing short of dropping
        // the decoder: a single 16 MiB packet would park 16 MiB for the rest of
        // the stream while `held_bytes` reported four. `to_vec` on the tail
        // allocates exactly the tail, and allocates
        // nothing at all when the packet ended on a segment boundary, which is
        // the overwhelmingly common case.
        self.carry = buffer[at..].to_vec();
        self.carry_rt = Some(rt);
        updates
    }

    /// One whole segment. `Err` is a malformed stream (a counted reset); `Ok`
    /// with an update is a closed display set.
    fn segment(
        &mut self,
        kind: u8,
        payload: &[u8],
        rt: gst::ClockTime,
    ) -> Result<Option<DisplayUpdate>, Fault> {
        if kind == SEGMENT_PRESENTATION {
            self.presentation_segment(payload, rt)?;
            return Ok(None);
        }
        // Everything else only means something inside a display set. Segments
        // between an END and the next presentation segment are skipped rather
        // than counted: joining a stream mid-set is normal (a seek lands
        // anywhere), and the reference draws the same line.
        if self.open.is_none() {
            debug!(kind, "pgs: a segment outside a display set");
            return Ok(None);
        }
        match kind {
            SEGMENT_PALETTE => self.palette_segment(payload)?,
            SEGMENT_WINDOW => window_segment(payload)?,
            SEGMENT_OBJECT => self.object_segment(payload)?,
            SEGMENT_INTERACTIVE => debug!("pgs: an interactive segment; menus are not subtitles"),
            SEGMENT_END => return self.close_set(),
            other => debug!(kind = other, "pgs: an unknown segment type"),
        }
        Ok(None)
    }

    /// The presentation segment: the canvas, and where each object goes on it.
    fn presentation_segment(&mut self, payload: &[u8], rt: gst::ClockTime) -> Result<(), Fault> {
        // 5 bytes of video descriptor, 3 of composition descriptor, then the
        // palette id, the flags and the object count.
        if payload.len() < 11 {
            return Err(Fault::Malformed(
                "a presentation segment shorter than its fixed header",
            ));
        }
        let canvas = (be16(&payload[0..2]), be16(&payload[2..4]));
        // payload[4] is the frame-rate code: display timing comes from the
        // packet's running time, so it is read past rather than used.
        //
        // payload[5..7] is the composition number and payload[7] the
        // COMPOSITION STATE (normal / acquisition point / epoch start). Ignored
        // deliberately, and the reference ignores it too: the state is a seek
        // aid for a player that wants to know which sets are self-contained,
        // and every set that reaches here is treated as self-contained anyway.
        // The engine's own epoch (a flush, a clear, a new stream) is what
        // resets this decoder, and it is more authoritative than a bit in a
        // stream that has just been seeked.
        //
        // payload[8]'s palette-update flag and payload[9]'s palette id are read
        // past for the same reason [`Self::palette_segment`] keeps one palette.
        let count = payload[10] as usize;

        let mut objects = Vec::with_capacity(count);
        let mut at = 11usize;
        for _ in 0..count {
            if at + 8 > payload.len() {
                return Err(Fault::Malformed(
                    "a composition object runs past its presentation segment",
                ));
            }
            let object_id = be16(&payload[at..at + 2]);
            let window_id = payload[at + 2];
            let flags = payload[at + 3];
            let x = be16(&payload[at + 4..at + 6]);
            let y = be16(&payload[at + 6..at + 8]);
            at += 8;
            let crop = if flags & COMPOSITION_FLAG_CROPPED != 0 {
                if at + 8 > payload.len() {
                    return Err(Fault::Malformed(
                        "a cropped composition object runs past its presentation segment",
                    ));
                }
                let crop = Crop {
                    x: be16(&payload[at..at + 2]),
                    y: be16(&payload[at + 2..at + 4]),
                    width: be16(&payload[at + 4..at + 6]),
                    height: be16(&payload[at + 6..at + 8]),
                };
                at += 8;
                Some(crop)
            } else {
                None
            };
            objects.push(CompositionObject {
                object_id,
                window_id,
                forced: flags & COMPOSITION_FLAG_FORCED != 0,
                x,
                y,
                crop,
            });
        }

        // A canvas is only needed to place something on it, so an empty set,
        // the clear, is accepted whatever it claims its canvas is.
        if !objects.is_empty() && (canvas.0 == 0 || canvas.1 == 0) {
            return Err(Fault::Malformed(
                "a presentation segment with objects and an empty canvas",
            ));
        }
        if self.open.is_some() {
            debug!("pgs: a new display set began before the previous one ended; dropping it");
        }
        // THE STORE FOLLOWS THE COMPOSITION. Objects survive across display
        // sets so that a palette-only update (the same picture, a new palette,
        // which is how the format fades a subtitle in and out) still has a
        // picture to colour. But only the objects the NEW composition names:
        // everything else is a picture nothing on screen refers to, and keeping
        // it would let a stream park its whole allocation budget in objects no
        // set will ever use and wedge every later subtitle behind the cap.
        self.objects
            .retain(|id, _| objects.iter().any(|object| object.object_id == *id));
        self.open = Some(OpenSet {
            rt,
            canvas,
            objects,
        });
        Ok(())
    }

    /// The palette segment: 5-byte YCbCr+A entries, expanded to straight-alpha
    /// RGBA here and nowhere else.
    ///
    /// ONE palette is kept, and the palette id is ignored. The reference makes
    /// the same call, and a stream that switches palette ids mid-epoch without
    /// re-sending the palette it wants is not one that has been seen in the
    /// field. A palette segment REPLACES the palette: entries it does not
    /// mention go fully transparent, which is what makes an update that shrinks
    /// the palette behave.
    fn palette_segment(&mut self, payload: &[u8]) -> Result<(), Fault> {
        if payload.len() < 2 {
            return Err(Fault::Malformed(
                "a palette segment shorter than its header",
            ));
        }
        // payload[0] palette id, payload[1] palette version.
        self.palette = [[0; 4]; 256];
        let mut at = 2usize;
        while at + PALETTE_ENTRY <= payload.len() {
            let entry = &payload[at..at + PALETTE_ENTRY];
            // Cr before Cb on the wire.
            self.palette[entry[0] as usize] = ycrcb_to_rgba(entry[1], entry[3], entry[2], entry[4]);
            at += PALETTE_ENTRY;
        }
        if at != payload.len() {
            // Tolerated rather than refused, as the reference tolerates it: a
            // ragged tail costs nothing and refusing would throw away a palette
            // that is otherwise entirely usable.
            debug!(
                trailing = payload.len() - at,
                "pgs: a palette segment with a ragged tail"
            );
        }
        Ok(())
    }

    /// The object segment: the RLE picture, possibly one fragment of it.
    ///
    /// Fragmentation is the format's own: an object bigger than a segment can
    /// carry (0xFFFF bytes) arrives as a first fragment declaring the 24-bit
    /// total, then continuations to append.
    fn object_segment(&mut self, payload: &[u8]) -> Result<(), Fault> {
        if payload.len() < 4 {
            return Err(Fault::Malformed(
                "an object segment shorter than its header",
            ));
        }
        let object_id = be16(&payload[0..2]);
        let version = payload[2];
        let flags = payload[3];
        let body = &payload[4..];

        // THE OPEN COMPOSITION IS THE GUEST LIST. An object segment for an id
        // the presentation segment did not name describes a picture no
        // composition will draw, and the reference refuses it for the same
        // reason (it can only find objects inside the current presentation
        // segment at all). Refusing it here is also what closes the budget
        // thrash: without it a stream can push
        // the store to the cap with objects nothing refers to, and every set
        // after that is refused by a budget spent on pictures for nobody.
        if !self
            .open
            .as_ref()
            .is_some_and(|set| set.objects.iter().any(|o| o.object_id == object_id))
        {
            debug!(
                object_id,
                "pgs: an object segment for an id the display set does not compose"
            );
            return Ok(());
        }

        if flags & OBJECT_FLAG_FIRST_FRAGMENT == 0 {
            let Some(object) = self.objects.get_mut(&object_id) else {
                // The first fragment never arrived (a mid-stream join, or a
                // first fragment this decoder refused). Skipped, not counted:
                // there is nothing to be corrupted.
                debug!(object_id, "pgs: a continuation for an unknown object");
                return Ok(());
            };
            if object.version != version {
                debug!(object_id, "pgs: a continuation for another object version");
                return Ok(());
            }
            if object.data.len() + body.len() > object.declared {
                return Err(Fault::Malformed(
                    "an object continuation past the end of the object it continues",
                ));
            }
            object.data.extend_from_slice(body);
            return Ok(());
        }

        if body.len() < 3 {
            return Err(Fault::Malformed(
                "an object segment with no declared length",
            ));
        }
        let declared = be24(&body[0..3]);
        let body = &body[3..];
        if declared == 0 {
            return Err(Fault::Malformed("an object segment declaring no data"));
        }
        if body.len() > declared {
            return Err(Fault::Malformed(
                "an object's first fragment longer than the object it declares",
            ));
        }
        // THE ALLOCATION SITE. The declared total is what this decoder is
        // committing to hold, so it is charged before a byte of it is kept.
        let held = self.object_bytes() - self.objects.get(&object_id).map_or(0, ObjectData::charge);
        if held + declared as u64 > WORKING_BUDGET {
            return Err(Fault::Budget(
                "an object store past the decoder's allocation budget",
            ));
        }
        // ALLOCATED TO `declared` UP FRONT, which is what makes the charge
        // above exact. Appending fragment by fragment into a growing `Vec`
        // doubles its capacity behind the accounting: an object charged 25 MiB
        // really held 50, and 65 000 tiny objects charged a byte each really
        // held eight. The budget check above has
        // already refused anything this allocation would not fit inside.
        let mut data = Vec::with_capacity(declared);
        data.extend_from_slice(body);
        self.objects.insert(
            object_id,
            ObjectData {
                version,
                declared,
                data,
            },
        );
        Ok(())
    }

    /// The END segment: turn the open display set into what should be on
    /// screen.
    fn close_set(&mut self) -> Result<Option<DisplayUpdate>, Fault> {
        // Only reachable with a set open (the guard in [`Self::segment`]), and
        // written so that a future caller which forgets that gets nothing
        // rather than a panic.
        let Some(set) = self.open.take() else {
            return Ok(None);
        };

        if set.objects.is_empty() {
            // THE SCHEDULED CLEAR. PGS takes a subtitle off the screen by
            // composing nothing, and this is that set: zero regions, at the
            // moment the set was presented.
            debug!(rt = %set.rt, "pgs: an empty display set clears the screen");
            self.failing = false;
            return Ok(Some(DisplayUpdate {
                start_rt: set.rt,
                end_rt: None,
                regions: Vec::new(),
            }));
        }

        // THE SECOND ALLOCATION SITE. Every RGBA page this set is about to
        // expand into, priced from the object headers before any of it is
        // allocated, and charged beside what the store already holds.
        //
        // A CROPPED object is priced for BOTH pages, and that is not
        // pessimism: the picture is expanded whole and then copied into a
        // second, smaller buffer, and for the duration of that copy the decoder
        // holds both. Pricing only the survivor
        // would let a set that fits the budget on paper allocate up to twice it
        // in practice.
        let mut pixels = 0u64;
        for object in &set.objects {
            let Some(data) = self.complete_object(object.object_id) else {
                continue;
            };
            let picture = picture_bytes(data);
            pixels = pixels.saturating_add(picture);
            if let Some(crop) = object.crop {
                pixels = pixels.saturating_add(crop_bytes(data, crop).min(picture));
            }
        }
        if self.object_bytes().saturating_add(pixels) > WORKING_BUDGET {
            return Err(Fault::Budget(
                "a display set past the decoder's allocation budget",
            ));
        }

        let mut regions = Vec::with_capacity(set.objects.len());
        let mut forced = false;
        // Whether any object the composition named was actually HERE. The
        // difference between "the stream has not sent these pictures" and "the
        // pictures it sent are broken" is the difference between a normal
        // mid-stream join and a corrupt stream, and only this loop can tell.
        let mut had_a_picture = false;
        for object in &set.objects {
            let Some(data) = self.complete_object(object.object_id) else {
                // An object the composition names and the stream never
                // completed. The set is still worth showing without it, since a
                // three-line subtitle missing its third line beats a blank
                // screen, so this is logged and not counted.
                debug!(
                    object_id = object.object_id,
                    window_id = object.window_id,
                    "pgs: a composition object with no complete picture behind it"
                );
                continue;
            };
            had_a_picture = true;
            let Some(picture) = decode_picture(data, &self.palette) else {
                debug!(
                    object_id = object.object_id,
                    "pgs: an object whose picture could not be decoded"
                );
                continue;
            };
            if let Some(region) = self.region_for(object, picture, set.canvas) {
                forced |= object.forced;
                regions.push(region);
            }
        }

        if regions.is_empty() {
            // NOT A CLEAR, either way: reporting an empty update here would
            // wipe a subtitle that is legitimately on screen. But the two ways
            // to get here are not the same event.
            //
            // Every named object ABSENT is the normal shape of a mid-stream
            // join: a seek lands inside an epoch and the next set is a
            // palette-only update naming pictures whose object segments went
            // past before the seek. Counting that as a malformed stream wiped
            // the palette and the store and cascaded into the sets behind it
            // which is the opposite of recovery.
            // It is the same event as one missing object, twelve lines up, and
            // it is logged the same way.
            //
            // An object that was PRESENT and produced nothing is a broken
            // picture, and that is a counted reset.
            if !had_a_picture {
                debug!(
                    rt = %set.rt,
                    objects = set.objects.len(),
                    "pgs: a display set composing only objects this decoder has never seen"
                );
                return Ok(None);
            }
            return Err(Fault::Malformed(
                "a display set whose composition objects produced no picture",
            ));
        }

        debug!(
            rt = %set.rt,
            regions = regions.len(),
            canvas = ?set.canvas,
            forced,
            "pgs: a display set"
        );
        self.failing = false;
        Ok(Some(DisplayUpdate {
            start_rt: set.rt,
            end_rt: None,
            regions,
        }))
    }

    /// The stored RLE data for an object, if it is complete and has a header.
    fn complete_object(&self, object_id: u16) -> Option<&[u8]> {
        let object = self.objects.get(&object_id)?;
        (object.data.len() == object.declared && object.data.len() >= 4).then_some(&object.data[..])
    }

    /// Place one decoded picture on the video, cropping it first if the
    /// composition asked for a part of it.
    fn region_for(
        &self,
        object: &CompositionObject,
        picture: Picture,
        canvas: (u16, u16),
    ) -> Option<BitmapRegion> {
        let Picture {
            width,
            height,
            pixels,
        } = match object.crop {
            Some(crop) => crop_picture(picture, crop)?,
            None => picture,
        };

        // FIT + CENTRE. Until the engine has taught a coded size, the canvas is
        // its own target: the regions then come out in canvas pixels, which is
        // the best available guess and exactly right for the common case where
        // the two agree.
        let (video_width, video_height) = self
            .video_size
            .unwrap_or((u32::from(canvas.0), u32::from(canvas.1)));
        let scale = f64::min(
            f64::from(video_width) / f64::from(canvas.0),
            f64::from(video_height) / f64::from(canvas.1),
        );
        let offset_x = (f64::from(video_width) - f64::from(canvas.0) * scale) / 2.0;
        let offset_y = (f64::from(video_height) - f64::from(canvas.1) * scale) / 2.0;

        let x = (offset_x + f64::from(object.x) * scale).round() as i32;
        let y = (offset_y + f64::from(object.y) * scale).round() as i32;
        // A composition may place an object off the picture entirely. The
        // format's positions are 16-bit and nothing in it says they have to fit
        // the canvas. Such a region cannot be seen, so it is not emitted:
        // the pixels would be uploaded and composited for nothing, and every
        // consumer of a region gets to assume it lands somewhere on the video.
        if x >= video_width as i32 || y >= video_height as i32 {
            debug!(
                object_id = object.object_id,
                x, y, "pgs: a composition object placed off the picture"
            );
            return None;
        }
        // The rect is CONTAINED in the picture, not merely started inside it.
        // A canvas smaller than the object it composes (a 1x1 canvas is
        // well-formed and costs two bytes to write) scales a four-pixel
        // picture to 4320 across a 1920-wide frame, and every consumer of a
        // region then has to decide what a rect hanging off the right edge
        // means. It means nothing anyone wants, so
        // it is clipped here, where the alternative shapes are still visible,
        // rather than in a compositor that will silently pick one.
        //
        // A region that scales to nothing is still a region: the `max(1)` keeps
        // a renderer from being handed a degenerate rectangle, and it is safe
        // because `x` and `y` are already strictly inside the picture.
        let render_width = ((f64::from(width) * scale).round() as u32)
            .min(video_width - x as u32)
            .max(1);
        let render_height = ((f64::from(height) * scale).round() as u32)
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

impl SubpicDecoder for PgsDecoder {
    /// PGS carries its palette in band; there is no out-of-band setup.
    fn set_codec_data(&mut self, _data: &[u8]) {}

    fn set_video_size(&mut self, width: u32, height: u32) {
        self.video_size = Some((width, height));
    }

    fn push(&mut self, packet: &BitmapPacket) -> Vec<DisplayUpdate> {
        let Ok(map) = packet.data.map_readable() else {
            self.fail(Fault::Malformed("a packet that could not be mapped"));
            return Vec::new();
        };
        self.consume(map.as_slice(), packet.rt)
    }

    /// Back to just-constructed, video size included. The engine re-teaches it
    /// before the next packet.
    ///
    /// `errors` deliberately survives: it is a counter the engine drains, not
    /// state the stream taught, and a reset between a failure and the drain
    /// would lose the very event the counter exists to report.
    fn reset(&mut self) {
        // Fresh allocations, not `clear`: see [`Self::fail`]. "Just-constructed"
        // has to mean the memory too, or a reset is a promise the heap does not
        // keep.
        self.carry = Vec::new();
        self.carry_rt = None;
        self.palette = [[0; 4]; 256];
        self.objects = HashMap::new();
        self.open = None;
        self.video_size = None;
        self.failing = false;
    }

    fn take_decode_errors(&mut self) -> u64 {
        std::mem::take(&mut self.errors)
    }
}

/// The window segment: validated, then dropped.
///
/// A window is the rectangle a group of objects is composited into, and it
/// exists so a player can repaint one strip of the screen. This decoder emits
/// one overlay per composition object and lets the renderer composite them, so
/// a window clips nothing that the object's own rectangle does not already clip
/// and the reference, which does parse windows, likewise renders from the
/// composition objects alone. Parsing it is still worth doing: a window segment
/// shorter than the windows it declares is a corrupt stream, and this is where
/// that is caught.
///
/// **Strict here, lenient in the palette, and that asymmetry is a decision**
/// A palette segment with a ragged tail has
/// delivered every entry it declared and has bytes left over: extra, harmless,
/// and tolerated, as the reference tolerates it. A window segment shorter than
/// its own count has NOT delivered what it declared, which means the framing is
/// wrong and whatever follows is not where it claims to be. Extra bytes are a
/// tolerance; missing bytes are a reset.
fn window_segment(payload: &[u8]) -> Result<(), Fault> {
    let Some((&count, rest)) = payload.split_first() else {
        return Err(Fault::Malformed("a window segment with no window count"));
    };
    // Per window: id u8, x/y/w/h u16be.
    if rest.len() < usize::from(count) * 9 {
        return Err(Fault::Malformed(
            "a window segment shorter than the windows it declares",
        ));
    }
    Ok(())
}

/// A decoded object picture: RGBA8, straight alpha, tightly packed.
struct Picture {
    width: u32,
    height: u32,
    pixels: Vec<u8>,
}

/// What an object's picture will cost in RGBA bytes, read off its header
/// WITHOUT decoding it, the whole point being to price the allocation before
/// making it.
fn picture_bytes(data: &[u8]) -> u64 {
    let width = u64::from(be16(&data[0..2]));
    let height = u64::from(be16(&data[2..4]));
    width * height * 4
}

/// What the CROPPED copy of an object's picture will cost, from the same
/// header. Clamped to the picture exactly as [`crop_picture`] clamps it, so the
/// price and the allocation are the same number.
fn crop_bytes(data: &[u8], crop: Crop) -> u64 {
    let width = u64::from(be16(&data[0..2]));
    let height = u64::from(be16(&data[2..4]));
    let x = u64::from(crop.x).min(width);
    let y = u64::from(crop.y).min(height);
    let cropped_width = u64::from(crop.width).min(width - x);
    let cropped_height = u64::from(crop.height).min(height - y);
    cropped_width * cropped_height * 4
}

/// Expand one object's RLE data into RGBA.
///
/// The four run codes are the format's, and the reference is the authority for
/// how a player treats the ragged edges of them: a zero-length run ends the
/// line, a run that would pass the right edge is clipped rather than wrapped,
/// and data after the last row is ignored. A truncated stream keeps whatever
/// rows it managed: a subtitle missing its last line is worth more than
/// nothing, and the caller has already refused to show a set that produced no
/// picture at all.
fn decode_picture(data: &[u8], palette: &[Rgba; 256]) -> Option<Picture> {
    let width = usize::from(be16(&data[0..2]));
    let height = usize::from(be16(&data[2..4]));
    if width == 0 || height == 0 {
        return None;
    }
    // The caller charged this against the budget before calling.
    let mut pixels = vec![0u8; width * height * 4];

    let mut at = 4usize;
    let (mut x, mut y) = (0usize, 0usize);
    while at < data.len() && y < height {
        let mut index = data[at];
        at += 1;
        let run = if index != 0 {
            1
        } else {
            let Some(&first) = data.get(at) else { break };
            match first & 0xC0 {
                0x00 => {
                    at += 1;
                    usize::from(first & 0x3F)
                }
                0x40 => {
                    let Some(&second) = data.get(at + 1) else {
                        break;
                    };
                    at += 2;
                    (usize::from(first) << 8 | usize::from(second)) & 0x3FFF
                }
                0x80 => {
                    let Some(&second) = data.get(at + 1) else {
                        break;
                    };
                    index = second;
                    at += 2;
                    usize::from(first & 0x3F)
                }
                _ => {
                    let (Some(&second), Some(&third)) = (data.get(at + 1), data.get(at + 2)) else {
                        break;
                    };
                    index = third;
                    at += 3;
                    (usize::from(first) << 8 | usize::from(second)) & 0x3FFF
                }
            }
        };

        if run == 0 {
            x = 0;
            y += 1;
            continue;
        }
        if x >= width {
            continue;
        }
        let run = run.min(width - x);
        let colour = palette[usize::from(index)];
        let row = (y * width + x) * 4;
        for pixel in 0..run {
            let at = row + pixel * 4;
            pixels[at..at + 4].copy_from_slice(&colour);
        }
        x += run;
    }

    Some(Picture {
        width: width as u32,
        height: height as u32,
        pixels,
    })
}

/// Cut the part of a picture a cropped composition object asks for. The crop is
/// clamped to the picture rather than refused: an authoring tool that names one
/// pixel too many should cost a pixel, not a subtitle.
fn crop_picture(picture: Picture, crop: Crop) -> Option<Picture> {
    let x = u32::from(crop.x).min(picture.width);
    let y = u32::from(crop.y).min(picture.height);
    let width = u32::from(crop.width).min(picture.width - x);
    let height = u32::from(crop.height).min(picture.height - y);
    if width == 0 || height == 0 {
        return None;
    }
    if (x, y, width, height) == (0, 0, picture.width, picture.height) {
        return Some(picture);
    }

    let (x, y) = (x as usize, y as usize);
    let (width, height) = (width as usize, height as usize);
    let source_stride = picture.width as usize * 4;
    let mut pixels = Vec::with_capacity(width * height * 4);
    for row in 0..height {
        let start = (y + row) * source_stride + x * 4;
        pixels.extend_from_slice(&picture.pixels[start..start + width * 4]);
    }
    Some(Picture {
        width: width as u32,
        height: height as u32,
        pixels,
    })
}

/// One palette entry, from the wire's YCbCr+A to the renderer's RGBA.
///
/// The coefficients are the reference's (`gstspu-pgs.c:530-533`): **BT.709**,
/// limited range, with the studio-swing expansion folded in, in fixed point:
/// `1.164 / 1.793 / -0.213 / -0.533 / 2.112` at 8 fractional bits. (BT.709 is
/// what the name says it is, and this comment said BT.601 until the
/// arithmetic was checked. The VALUES were always the reference's, and
/// the reference is right, since Blu-ray video is BT.709 and its graphics
/// planes are authored to match. A provenance comment that names the wrong
/// standard is a provenance comment nobody can check.) They are cited rather
/// than re-derived because "which matrix does a Blu-ray palette actually use"
/// is a question the format leaves open and twenty years of field use answers.
///
/// **The alpha stays STRAIGHT.** The reference premultiplies here
/// (`gstspu-pgs.c:545-548`) because its compositor wants that; this renderer
/// composites with independent alpha, and a premultiplied palette would wash
/// every semi-transparent pixel toward black. The canary test pins it.
fn ycrcb_to_rgba(y: u8, cb: u8, cr: u8, alpha: u8) -> Rgba {
    let (y, cb, cr) = (i32::from(y), i32::from(cb), i32::from(cr));
    let r = (298 * y + 459 * cr - 63514) >> 8;
    let g = (298 * y - 55 * cb - 136 * cr + 19681) >> 8;
    let b = (298 * y + 541 * cb - 73988) >> 8;
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

fn be24(bytes: &[u8]) -> usize {
    usize::from(bytes[0]) << 16 | usize::from(bytes[1]) << 8 | usize::from(bytes[2])
}

/// Hand-crafted display sets: the format's own bytes, written out field by
/// field.
///
/// **Why this ships in the library.** There is no PGS encoder anywhere, not in
/// GStreamer and not in ffmpeg, so a PGS test vector is bytes somebody wrote by
/// hand, and three different places need the same ones: this file's unit
/// vectors, the fuzz targets' known-good tail (a decoder that stops decoding
/// after garbage is a defect the fuzzer must be able to see), and the fuzz seed
/// corpus. A `cfg(test)` module is invisible to `fuzz/` and to `tests/`, which
/// link this crate the way any dependent does, the same reason
/// `CueEngine::set_decoder_factory` is doc-hidden rather than test-only.
///
/// Not part of the public API: doc-hidden, and nothing in production calls it.
#[doc(hidden)]
pub mod fixtures {
    use super::{COMPOSITION_FLAG_CROPPED, COMPOSITION_FLAG_FORCED, OBJECT_FLAG_FIRST_FRAGMENT};

    pub use super::{
        SEGMENT_END, SEGMENT_INTERACTIVE, SEGMENT_OBJECT, SEGMENT_PALETTE, SEGMENT_PRESENTATION,
        SEGMENT_WINDOW,
    };

    /// `[type u8][length u16be][payload]`.
    pub fn segment(kind: u8, payload: &[u8]) -> Vec<u8> {
        let mut out = vec![kind];
        out.extend_from_slice(&(payload.len().min(0xFFFF) as u16).to_be_bytes());
        out.extend_from_slice(payload);
        out
    }

    pub fn joined(parts: &[Vec<u8>]) -> Vec<u8> {
        parts.concat()
    }

    /// One entry of a presentation segment's composition.
    #[derive(Clone, Copy)]
    pub struct Composed {
        pub id: u16,
        pub x: u16,
        pub y: u16,
        pub forced: bool,
        pub crop: Option<(u16, u16, u16, u16)>,
    }

    pub fn composed(id: u16, x: u16, y: u16) -> Composed {
        Composed {
            id,
            x,
            y,
            forced: false,
            crop: None,
        }
    }

    pub fn presentation(canvas: (u16, u16), objects: &[Composed]) -> Vec<u8> {
        let mut payload = Vec::new();
        payload.extend_from_slice(&canvas.0.to_be_bytes());
        payload.extend_from_slice(&canvas.1.to_be_bytes());
        payload.push(0x10); // frame rate code
        payload.extend_from_slice(&7u16.to_be_bytes()); // composition number
        payload.push(0x80); // composition state: epoch start, ignored
        payload.push(0x00); // flags
        payload.push(0x00); // palette id
        payload.push(objects.len() as u8);
        for object in objects {
            payload.extend_from_slice(&object.id.to_be_bytes());
            payload.push(0); // window id
            let mut flags = 0u8;
            if object.forced {
                flags |= COMPOSITION_FLAG_FORCED;
            }
            if object.crop.is_some() {
                flags |= COMPOSITION_FLAG_CROPPED;
            }
            payload.push(flags);
            payload.extend_from_slice(&object.x.to_be_bytes());
            payload.extend_from_slice(&object.y.to_be_bytes());
            if let Some((x, y, width, height)) = object.crop {
                for value in [x, y, width, height] {
                    payload.extend_from_slice(&value.to_be_bytes());
                }
            }
        }
        segment(SEGMENT_PRESENTATION, &payload)
    }

    pub fn window(id: u8, x: u16, y: u16, width: u16, height: u16) -> Vec<u8> {
        let mut payload = vec![1, id];
        for value in [x, y, width, height] {
            payload.extend_from_slice(&value.to_be_bytes());
        }
        segment(SEGMENT_WINDOW, &payload)
    }

    /// `(index, Y, Cr, Cb, A)`, which is the order the wire uses.
    pub fn palette(entries: &[(u8, u8, u8, u8, u8)]) -> Vec<u8> {
        let mut payload = vec![0, 0]; // palette id, version
        for (index, y, cr, cb, alpha) in entries {
            payload.extend_from_slice(&[*index, *y, *cr, *cb, *alpha]);
        }
        segment(SEGMENT_PALETTE, &payload)
    }

    pub fn object_first(id: u16, version: u8, total: usize, body: &[u8]) -> Vec<u8> {
        let mut payload = id.to_be_bytes().to_vec();
        payload.push(version);
        payload.push(OBJECT_FLAG_FIRST_FRAGMENT);
        payload.extend_from_slice(&[(total >> 16) as u8, (total >> 8) as u8, total as u8]);
        payload.extend_from_slice(body);
        segment(SEGMENT_OBJECT, &payload)
    }

    pub fn object_more(id: u16, version: u8, body: &[u8]) -> Vec<u8> {
        let mut payload = id.to_be_bytes().to_vec();
        payload.push(version);
        payload.push(0);
        payload.extend_from_slice(body);
        segment(SEGMENT_OBJECT, &payload)
    }

    pub fn end() -> Vec<u8> {
        segment(SEGMENT_END, &[])
    }

    /// One RLE run, in whichever of the four codes fits it.
    pub fn run(index: u8, length: u16) -> Vec<u8> {
        match (index, length) {
            (0, length) if length < 64 => vec![0, length as u8],
            (0, length) => vec![0, 0x40 | (length >> 8) as u8, length as u8],
            (index, 1) => vec![index],
            (index, length) if length < 64 => vec![0, 0x80 | length as u8, index],
            (index, length) => vec![0, 0xC0 | (length >> 8) as u8, length as u8, index],
        }
    }

    /// An object's RLE data: the `width, height` header, then the rows, each
    /// closed by the zero-length run that means end of line.
    pub fn picture_data(width: u16, height: u16, rows: &[&[(u8, u16)]]) -> Vec<u8> {
        let mut out = width.to_be_bytes().to_vec();
        out.extend_from_slice(&height.to_be_bytes());
        for row in rows {
            for (index, length) in row.iter() {
                out.extend_from_slice(&run(*index, *length));
            }
            out.extend_from_slice(&[0, 0]);
        }
        out
    }

    /// THE MINIMAL DISPLAY SET: a 4x2 picture at (100, 200) on a 1920x1080
    /// canvas, drawn from three palette entries: opaque white,
    /// HALF-TRANSPARENT white (the straight-alpha canary) and a saturated red
    /// (which pins the colour matrix's channel routing, since Cr drives red and
    /// Cb drives blue).
    pub fn minimal_display_set() -> Vec<u8> {
        joined(&[
            presentation((1920, 1080), &[composed(1, 100, 200)]),
            window(0, 100, 200, 4, 2),
            palette(&[
                (1, 235, 128, 128, 255),
                (2, 235, 128, 128, 128),
                (3, 81, 240, 90, 255),
            ]),
            {
                let data = picture_data(4, 2, &[&[(3, 1), (1, 3)], &[(2, 2), (0, 2)]]);
                object_first(1, 0, data.len(), &data)
            },
            end(),
        ])
    }

    /// The same set with its object split in two, as two packets' worth of
    /// bytes. Both halves carry the same running time, as every packet of one
    /// display set does.
    pub fn fragmented_display_set() -> (Vec<u8>, Vec<u8>) {
        let data = picture_data(4, 2, &[&[(1, 4)], &[(1, 4)]]);
        let (head, tail) = data.split_at(6);
        (
            joined(&[
                presentation((1920, 1080), &[composed(1, 0, 0)]),
                palette(&[(1, 235, 128, 128, 255)]),
                object_first(1, 0, data.len(), head),
            ]),
            joined(&[object_more(1, 0, tail), end()]),
        )
    }

    /// The scheduled clear: a display set that composes nothing.
    pub fn empty_display_set() -> Vec<u8> {
        joined(&[presentation((1920, 1080), &[]), end()])
    }

    /// A presentation segment too short to hold its own fixed header.
    pub fn truncated_presentation() -> Vec<u8> {
        joined(&[segment(SEGMENT_PRESENTATION, &[0, 0, 0, 0]), end()])
    }

    /// An object whose first fragment carries more bytes than the object it
    /// says it is starting.
    pub fn oversized_object() -> Vec<u8> {
        let data = picture_data(4, 2, &[&[(1, 4)], &[(1, 4)]]);
        joined(&[
            presentation((1920, 1080), &[composed(1, 0, 0)]),
            object_first(1, 0, data.len() - 5, &data),
            end(),
        ])
    }

    /// A picture whose header asks for 64 MiB of RGBA out of a few dozen input
    /// bytes, the allocation cap's own vector.
    pub fn oversized_picture() -> Vec<u8> {
        let data = picture_data(4096, 4096, &[&[(1, 1)]]);
        joined(&[
            presentation((1920, 1080), &[composed(1, 0, 0)]),
            palette(&[(1, 235, 128, 128, 255)]),
            object_first(1, 0, data.len(), &data),
            end(),
        ])
    }

    /// Every vector above, named, for a fuzz seed corpus.
    pub fn seed_corpus() -> Vec<(&'static str, Vec<u8>)> {
        let (head, tail) = fragmented_display_set();
        vec![
            ("minimal", minimal_display_set()),
            ("fragment-head", head),
            ("fragment-tail", tail),
            ("empty-clear", empty_display_set()),
            ("truncated-presentation", truncated_presentation()),
            ("oversized-object", oversized_object()),
            ("oversized-picture", oversized_picture()),
            (
                "cropped",
                joined(&[
                    presentation(
                        (1920, 1080),
                        &[Composed {
                            crop: Some((2, 0, 2, 1)),
                            ..composed(1, 10, 20)
                        }],
                    ),
                    palette(&[(1, 235, 128, 128, 255), (3, 81, 240, 90, 255)]),
                    {
                        let data = picture_data(4, 2, &[&[(1, 2), (3, 2)], &[(3, 4)]]);
                        object_first(1, 0, data.len(), &data)
                    },
                    end(),
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

    fn packet(bytes: &[u8], rt_ms: u64) -> BitmapPacket {
        BitmapPacket {
            format: BitmapFormat::Pgs,
            data: gst::Buffer::from_slice(bytes.to_vec()),
            codec_data: None,
            rt: gst::ClockTime::from_mseconds(rt_ms),
            duration: None,
        }
    }

    fn joined(parts: &[Vec<u8>]) -> Vec<u8> {
        parts.concat()
    }

    /// THE MINIMAL DISPLAY SET, used by several tests: a 4x2 picture at
    /// (100, 200) on a 1920x1080 canvas, drawn from three palette entries:
    /// opaque white, HALF-TRANSPARENT white (the straight-alpha canary) and a
    /// saturated red (which pins the colour matrix's channel routing, since Cr
    /// drives red and Cb drives blue).
    fn minimal_set() -> Vec<u8> {
        joined(&[
            presentation((1920, 1080), &[composed(1, 100, 200)]),
            window(0, 100, 200, 4, 2),
            palette(&[
                (1, 235, 128, 128, 255),
                (2, 235, 128, 128, 128),
                (3, 81, 240, 90, 255),
            ]),
            {
                let data = picture_data(4, 2, &[&[(3, 1), (1, 3)], &[(2, 2), (0, 2)]]);
                object_first(1, 0, data.len(), &data)
            },
            end(),
        ])
    }

    fn pixel(region: &BitmapRegion, x: u32, y: u32) -> Rgba {
        let at = ((y * region.width + x) * 4) as usize;
        region.pixels[at..at + 4]
            .try_into()
            .expect("four bytes of RGBA")
    }

    fn decode_one(decoder: &mut PgsDecoder, bytes: &[u8], rt_ms: u64) -> Vec<DisplayUpdate> {
        decoder.push(&packet(bytes, rt_ms))
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

    // ------------------------------------------------------------ U-PGS

    /// The vector this whole decoder exists for: one display set in, one region
    /// out, with the pixels named.
    ///
    /// THE STRAIGHT-ALPHA CANARY is the assertion on the half-transparent
    /// entry. The reference implementation premultiplies its palette by alpha
    /// (`gstspu-pgs.c:545-548`), which would make that pixel `[127, 127, 127,
    /// 128]`; this renderer composites with independent alpha, so the colour
    /// must arrive UNSCALED and only the alpha channel may say it is
    /// half-transparent. It is the one place where copying the reference would
    /// have produced code that looks right and renders wrong on every
    /// anti-aliased subtitle edge in existence.
    #[test]
    fn a_minimal_display_set_becomes_one_region_with_straight_alpha() {
        gst::init().unwrap();
        let mut decoder = PgsDecoder::new();
        decoder.set_video_size(1920, 1080);

        let updates = decode_one(&mut decoder, &minimal_display_set(), 1_000);

        assert_eq!(updates.len(), 1, "one display set is one update");
        let update = &updates[0];
        assert_eq!(update.start_rt, gst::ClockTime::from_mseconds(1_000));
        assert_eq!(
            update.end_rt, None,
            "a PGS set has no end of its own; it lives until it is superseded"
        );
        assert_eq!(update.regions.len(), 1);

        let region = &update.regions[0];
        assert_eq!(
            (region.width, region.height),
            (4, 2),
            "the texture is native"
        );
        assert_eq!(
            (
                region.x,
                region.y,
                region.render_width,
                region.render_height
            ),
            (100, 200, 4, 2),
            "canvas and video agree, so the rect is the composition's own"
        );
        assert_eq!(
            pixel(region, 0, 0),
            [255, 24, 0, 255],
            "Cr must drive red: the colour matrix is wired up wrong"
        );
        assert_eq!(pixel(region, 1, 0), [254, 254, 255, 255], "opaque white");
        assert_eq!(
            pixel(region, 0, 1),
            [254, 254, 255, 128],
            "THE CANARY: a half-transparent palette entry came out premultiplied"
        );
        assert_eq!(
            pixel(region, 2, 1),
            [0, 0, 0, 0],
            "a run of palette entry 0 is transparent"
        );
        assert_eq!(decoder.take_decode_errors(), 0);
        assert_eq!(decoder.budget_resets(), 0);
    }

    /// An object bigger than one segment arrives in fragments, and the
    /// fragments arrive in different PACKETS, so this is the display set's
    /// cross-buffer accumulation and the object's own reassembly at once.
    #[test]
    fn an_object_fragmented_across_two_packets_decodes_whole() {
        gst::init().unwrap();
        let mut decoder = PgsDecoder::new();
        decoder.set_video_size(1920, 1080);

        let data = picture_data(4, 2, &[&[(1, 4)], &[(1, 4)]]);
        let (head, tail) = data.split_at(6);

        let first = joined(&[
            presentation((1920, 1080), &[composed(1, 0, 0)]),
            palette(&[(1, 235, 128, 128, 255)]),
            object_first(1, 0, data.len(), head),
        ]);
        assert!(
            decode_one(&mut decoder, &first, 500).is_empty(),
            "a set with an incomplete object must not reach the screen"
        );

        let second = joined(&[object_more(1, 0, tail), end()]);
        let updates = decode_one(&mut decoder, &second, 500);

        assert_eq!(updates.len(), 1, "the completed set");
        let region = &updates[0].regions[0];
        assert_eq!((region.width, region.height), (4, 2));
        assert_eq!(
            pixel(region, 3, 1),
            [254, 254, 255, 255],
            "the last pixel comes from the second fragment's bytes"
        );
        assert_eq!(decoder.take_decode_errors(), 0);
    }

    /// A segment split MID-HEADER, the byte-level half of the framing, which
    /// no object fragmentation exercises: three bytes of a segment header are
    /// not a segment, and must wait rather than be misread as one.
    #[test]
    fn a_segment_split_between_packets_is_carried_and_completed() {
        gst::init().unwrap();
        let mut decoder = PgsDecoder::new();
        decoder.set_video_size(1920, 1080);

        let whole = minimal_set();
        // Two bytes into the END segment's own header: the split lands inside a
        // length field, which is the case a naive parser reads as a length.
        let cut = whole.len() - 1;
        let (first, second) = whole.split_at(cut);

        assert!(
            decode_one(&mut decoder, first, 250).is_empty(),
            "a half-delivered segment closed a display set"
        );
        assert_eq!(decoder.take_decode_errors(), 0, "waiting is not failing");

        let updates = decode_one(&mut decoder, second, 250);
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].regions.len(), 1);
    }

    /// A reset drops the half-assembled set whole, in BOTH of the two things a
    /// display set can be half of: an object waiting for its remaining
    /// fragments, and a segment waiting for the rest of its own bytes. Neither
    /// may reach the screen after the flush that dropped it.
    #[test]
    fn a_partial_set_is_dropped_on_reset() {
        gst::init().unwrap();
        let mut decoder = PgsDecoder::new();
        decoder.set_video_size(1920, 1080);

        let data = picture_data(4, 2, &[&[(1, 4)], &[(1, 4)]]);
        let (head, tail) = data.split_at(6);
        let first = joined(&[
            presentation((1920, 1080), &[composed(1, 0, 0)]),
            palette(&[(1, 235, 128, 128, 255)]),
            object_first(1, 0, data.len(), head),
        ]);
        assert!(decode_one(&mut decoder, &first, 500).is_empty());
        assert_eq!(
            decoder.held_bytes(),
            data.len() as u64,
            "the half-delivered object is what the decoder is holding"
        );

        decoder.reset();
        assert_eq!(
            decoder.held_bytes(),
            0,
            "a reset must return the decoder to its just-constructed state, holding nothing"
        );

        let second = joined(&[object_more(1, 0, tail), end()]);
        assert!(
            decode_one(&mut decoder, &second, 500).is_empty(),
            "a set from before the reset came out after it"
        );
        assert_eq!(
            decoder.take_decode_errors(),
            0,
            "a reset is not an error, and neither is the orphaned tail behind it"
        );

        // And the decoder still works: the next whole set decodes.
        decoder.set_video_size(1920, 1080);
        assert_eq!(
            decode_one(&mut decoder, &minimal_display_set(), 900).len(),
            1
        );

        // THE OTHER HALF: a reset with bytes CARRIED, i.e. mid-segment rather
        // than mid-object. The split is two bytes into the display set's very
        // first segment, so with those two bytes still carried the set decodes
        // whole, and it must not, because the flush that dropped them said the
        // stream before it no longer exists.
        let whole = minimal_display_set();
        let (head, tail) = whole.split_at(2);
        assert!(decode_one(&mut decoder, head, 1_500).is_empty());
        decoder.reset();
        decoder.set_video_size(1920, 1080);
        assert!(
            decode_one(&mut decoder, tail, 1_500).is_empty(),
            "a display set from before the reset decoded after it: the two bytes of its first \
             segment were still being carried"
        );
        assert_eq!(
            decode_one(&mut decoder, &minimal_display_set(), 2_000).len(),
            1,
            "the decoder never recovered from the reset"
        );
    }

    /// The scheduled clear. PGS takes a subtitle off the screen with a display
    /// set that composes NOTHING, and that is a zero-region update at its own
    /// running time, not a decode failure and not silence.
    #[test]
    fn an_empty_presentation_segment_is_the_scheduled_clear() {
        gst::init().unwrap();
        let mut decoder = PgsDecoder::new();
        decoder.set_video_size(1920, 1080);
        assert_eq!(
            decode_one(&mut decoder, &minimal_display_set(), 1_000).len(),
            1
        );

        let clear = joined(&[presentation((1920, 1080), &[]), end()]);
        let updates = decode_one(&mut decoder, &clear, 4_000);

        assert_eq!(updates.len(), 1);
        assert!(
            updates[0].regions.is_empty(),
            "the clear must be an update with no regions, not no update"
        );
        assert_eq!(updates[0].start_rt, gst::ClockTime::from_mseconds(4_000));
        assert_eq!(decoder.take_decode_errors(), 0);
    }

    /// FIT + CENTRE, at two video sizes against one authoring canvas.
    ///
    /// The 1280x720 case has the same aspect as the canvas, so it is a pure
    /// scale. The 1000x1000 case does not, and is the one that says which
    /// policy this is: the picture is letterboxed inside the video, so the
    /// subtitle moves DOWN by half the letterbox rather than being stretched
    /// away from the feature it annotates.
    #[test]
    fn the_canvas_is_fitted_and_centred_on_the_video() {
        gst::init().unwrap();

        let region_at = |size: Option<(u32, u32)>| {
            let mut decoder = PgsDecoder::new();
            if let Some((width, height)) = size {
                decoder.set_video_size(width, height);
            }
            let updates = decode_one(&mut decoder, &minimal_display_set(), 0);
            let region = &updates[0].regions[0];
            (
                region.x,
                region.y,
                region.render_width,
                region.render_height,
            )
        };

        assert_eq!(
            region_at(Some((1280, 720))),
            (67, 133, 3, 1),
            "same aspect: a pure scale, no offset"
        );
        assert_eq!(
            region_at(Some((1000, 1000))),
            (52, 323, 2, 1),
            "taller than the canvas: centred inside the letterbox"
        );
        // The case that says FIT rather than "scale by the width": here the
        // HEIGHT is the binding dimension, so a decoder that scaled by the
        // width alone would put the subtitle a full picture below the bottom of
        // the frame.
        assert_eq!(
            region_at(Some((1920, 540))),
            (530, 100, 2, 1),
            "wider than the canvas: the height binds and the picture is pillarboxed"
        );
        assert_eq!(
            region_at(None),
            (100, 200, 4, 2),
            "with no video size taught, the canvas is its own target"
        );
    }

    /// Malformed input is a counted reset and never a panic, in the three
    /// shapes the framing can go wrong in.
    #[test]
    fn malformed_segments_are_counted_resets_and_never_panics() {
        gst::init().unwrap();

        // A presentation segment too short to hold its own fixed header.
        let mut decoder = PgsDecoder::new();
        let truncated = joined(&[segment(SEGMENT_PRESENTATION, &[0, 0, 0, 0]), end()]);
        assert!(decode_one(&mut decoder, &truncated, 0).is_empty());
        assert_eq!(decoder.take_decode_errors(), 1);

        // A composition claiming more objects than its segment carries.
        let mut decoder = PgsDecoder::new();
        let mut payload = presentation((1920, 1080), &[composed(1, 0, 0)]);
        payload[3 + 10] = 4; // the object count, against one object of bytes
        assert!(decode_one(&mut decoder, &payload, 0).is_empty());
        assert_eq!(decoder.take_decode_errors(), 1);

        // An object whose first fragment carries more bytes than the object it
        // says it is starting.
        let mut decoder = PgsDecoder::new();
        let data = picture_data(4, 2, &[&[(1, 4)], &[(1, 4)]]);
        let oversized = joined(&[
            presentation((1920, 1080), &[composed(1, 0, 0)]),
            object_first(1, 0, data.len() - 5, &data),
            end(),
        ]);
        assert!(decode_one(&mut decoder, &oversized, 0).is_empty());
        assert_eq!(decoder.take_decode_errors(), 1);
        assert_eq!(
            decoder.budget_resets(),
            0,
            "a malformed stream is not a budget breach"
        );

        // Random bytes, at length, must not panic and must not WEDGE either.
        // Noise ends mid-segment by construction, and the bytes carried over
        // from it would otherwise eat the next display set as their payload.
        let mut decoder = PgsDecoder::new();
        decoder.set_video_size(1920, 1080);
        let mut noise = Vec::new();
        for index in 0..4096u32 {
            noise.push((index.wrapping_mul(2_654_435_761) >> 13) as u8);
        }
        decode_one(&mut decoder, &noise, 0);
        assert_eq!(
            decode_one(&mut decoder, &minimal_display_set(), 100).len(),
            1,
            "a display set after the noise never decoded: the carry wedged the stream"
        );
    }

    /// The allocation cap, breached deliberately in both places it is
    /// charged: a picture whose header asks for 64 MiB of RGBA from a handful
    /// of input bytes, and an object store whose declared totals add up past
    /// the budget. Both are counted resets, and neither allocates what was
    /// asked for.
    #[test]
    fn an_allocation_past_the_budget_is_a_counted_reset() {
        gst::init().unwrap();

        // 4096x4096 RGBA is 64 MiB, from a display set of about sixty bytes.
        let mut decoder = PgsDecoder::new();
        decoder.set_video_size(1920, 1080);
        let data = picture_data(4096, 4096, &[&[(1, 1)]]);
        let huge = joined(&[
            presentation((1920, 1080), &[composed(1, 0, 0)]),
            palette(&[(1, 235, 128, 128, 255)]),
            object_first(1, 0, data.len(), &data),
            end(),
        ]);
        assert!(
            decode_one(&mut decoder, &huge, 0).is_empty(),
            "a picture past the budget must not be built"
        );
        assert_eq!(decoder.budget_resets(), 1);
        assert_eq!(decoder.take_decode_errors(), 1);

        // Three objects, each declaring the 24-bit maximum. Two of them are
        // 33 554 430 bytes, which is 32 MiB less two, but the carry's reserve
        // comes out of the budget first, so the SECOND is already the one that
        // does not fit. The assertion is on the counter rather than on which
        // object broke it, because the reserve is an implementation choice and
        // the cap is the contract.
        //
        // NO END SEGMENT, deliberately: the display set's own check would
        // otherwise catch the same overrun a moment later, and this half is
        // about the store's charge at the moment an object is ACCEPTED. What it
        // observes is what the decoder is holding, which is the cap's actual
        // subject.
        let mut decoder = PgsDecoder::new();
        decoder.set_video_size(1920, 1080);
        let biggest = 0xFF_FFFF;
        let mut store = presentation(
            (1920, 1080),
            &[composed(1, 0, 0), composed(2, 0, 0), composed(3, 0, 0)],
        );
        for id in 1..=3u16 {
            store.extend_from_slice(&object_first(id, 0, biggest, &[0, 4, 0, 4]));
        }
        assert!(decode_one(&mut decoder, &store, 0).is_empty());
        assert!(
            decoder.held_bytes() <= ALLOCATION_BUDGET,
            "the decoder is holding {} bytes against a budget of {ALLOCATION_BUDGET}",
            decoder.held_bytes()
        );
        assert_eq!(
            decoder.budget_resets(),
            1,
            "the object store's own charge never fired"
        );
        assert_eq!(decoder.take_decode_errors(), 1);

        // Recovery: the cap resets the decoder, it does not disable it.
        decoder.set_video_size(1920, 1080);
        assert_eq!(
            decode_one(&mut decoder, &minimal_display_set(), 100).len(),
            1
        );
        assert_eq!(decoder.take_decode_errors(), 0);
    }

    /// A cropped composition object shows the part of the picture it names, at
    /// the composition's position.
    #[test]
    fn a_cropped_object_shows_the_part_it_names() {
        gst::init().unwrap();
        let mut decoder = PgsDecoder::new();
        decoder.set_video_size(1920, 1080);

        let data = picture_data(4, 2, &[&[(1, 2), (3, 2)], &[(3, 4)]]);
        let cropped = Composed {
            crop: Some((2, 0, 2, 1)),
            ..composed(1, 10, 20)
        };
        let bytes = joined(&[
            presentation((1920, 1080), &[cropped]),
            palette(&[(1, 235, 128, 128, 255), (3, 81, 240, 90, 255)]),
            object_first(1, 0, data.len(), &data),
            end(),
        ]);

        let updates = decode_one(&mut decoder, &bytes, 0);
        let region = &updates[0].regions[0];
        assert_eq!((region.width, region.height), (2, 1), "the cropped texture");
        assert_eq!(
            (region.x, region.y),
            (10, 20),
            "a crop moves what is shown, not where it is shown"
        );
        assert_eq!(
            pixel(region, 0, 0),
            [255, 24, 0, 255],
            "the crop starts at the third column, which is red"
        );
    }

    /// The carry's CAPACITY is an allocation, and it is neither charged
    /// nor released by anything short of dropping the decoder unless the buffer
    /// is rebuilt.
    ///
    /// The defect this pins was invisible to every other test in this file,
    /// because `held_bytes` counted the carry's LENGTH: a 16 MiB packet ending
    /// mid-segment reported four bytes held and kept sixteen megabytes, through
    /// `fail`, through `reset`, for the life of the decoder. What the assertion
    /// reads is the same number the fuzz target reads after every packet, which
    /// is why the fix had to be in the accounting and not only in the free.
    #[test]
    fn a_huge_packet_does_not_park_its_buffer_in_the_carry() {
        gst::init().unwrap();
        let mut decoder = PgsDecoder::new();
        decoder.set_video_size(1920, 1080);

        // 16 MiB whose last segment is deliberately unfinished, so the carry is
        // non-empty and the scratch buffer it came from is enormous.
        let mut huge = Vec::with_capacity(16 * 1024 * 1024);
        while huge.len() < 16 * 1024 * 1024 - 8 {
            huge.extend_from_slice(&segment(SEGMENT_INTERACTIVE, &[0; 64]));
        }
        huge.extend_from_slice(&[SEGMENT_PRESENTATION, 0xFF, 0xFF, 0x01, 0x02]);
        let carried = 5;
        assert!(
            huge.len() > 8 * 1024 * 1024,
            "the vector under test is real"
        );

        decode_one(&mut decoder, &huge, 0);
        assert_eq!(
            decoder.held_bytes(),
            carried,
            "the carry is holding the whole packet's buffer, not the bytes it carried"
        );

        // And a reset gives even those back.
        decoder.reset();
        assert_eq!(
            decoder.held_bytes(),
            0,
            "a reset kept the carry's allocation"
        );

        // The same for the failure path, which is the one a hostile stream
        // takes: fill the carry, then break the stream.
        decoder.set_video_size(1920, 1080);
        decode_one(&mut decoder, &huge, 100);
        assert_eq!(decoder.held_bytes(), carried);
        decode_one(&mut decoder, &truncated_presentation(), 200);
        assert_eq!(
            decoder.held_bytes(),
            0,
            "a counted reset kept the carry's allocation"
        );
    }

    /// An object is charged what it ALLOCATES, never less.
    ///
    /// The store used to charge the declared length while the buffer behind it
    /// grew by doubling, so an object charged a megabyte really held two by the
    /// time its last fragment arrived, and no number the decoder reported said
    /// so, which is why it took a counting allocator to find. The assertion
    /// that matters is the SECOND one: the accounting may never be smaller than
    /// the allocation, whatever either of them is.
    #[test]
    fn an_object_is_charged_what_it_allocates() {
        gst::init().unwrap();
        let mut decoder = PgsDecoder::new();
        decoder.set_video_size(1920, 1080);

        let declared = 1_000_000usize;
        let mut open = presentation((1920, 1080), &[composed(1, 0, 0)]);
        open.extend_from_slice(&object_first(1, 0, declared, &[0, 4, 0, 4]));
        decode_one(&mut decoder, &open, 0);

        let check = |decoder: &PgsDecoder, at: &str| {
            assert!(
                decoder.held_bytes() >= decoder.allocated_bytes(),
                "{at}: the decoder is holding {} bytes and has charged itself {}",
                decoder.allocated_bytes(),
                decoder.held_bytes()
            );
            assert_eq!(
                decoder.held_bytes(),
                declared as u64,
                "{at}: an object's charge moved off its declared length"
            );
        };
        check(&decoder, "the first fragment");

        // DELIVERED TO COMPLETION, which is where a growing buffer's last
        // reallocation lands: the append that fills the object is the one that
        // doubles past it.
        let mut delivered = 4usize;
        while delivered < declared {
            let chunk = vec![0u8; (declared - delivered).min(60_000)];
            delivered += chunk.len();
            decode_one(&mut decoder, &object_more(1, 0, &chunk), 0);
            check(&decoder, "a continuation");
        }
        assert_eq!(delivered, declared, "the object is complete");
    }

    /// The other half: a CROPPED object is priced for both pages it
    /// briefly holds.
    ///
    /// The picture is expanded whole and then copied into a smaller buffer, and
    /// for the length of that copy both are alive. Sized here so that the
    /// picture alone fits the budget and the pair does not: pricing only the
    /// survivor lets this set through, and the decoder then allocates twice
    /// what it was allowed to.
    #[test]
    fn a_cropped_object_is_priced_for_the_copy_it_makes() {
        gst::init().unwrap();
        let mut decoder = PgsDecoder::new();
        decoder.set_video_size(1920, 1080);

        // 2048x2048 RGBA is 16 MiB, half the budget, and the crop asks for all
        // of it a second time.
        let data = picture_data(2048, 2048, &[&[(1, 1)]]);
        let cropped = Composed {
            crop: Some((0, 0, 2048, 2048)),
            ..composed(1, 0, 0)
        };
        let bytes = joined(&[
            presentation((1920, 1080), &[cropped]),
            palette(&[(1, 235, 128, 128, 255)]),
            object_first(1, 0, data.len(), &data),
            end(),
        ]);

        assert!(
            decode_one(&mut decoder, &bytes, 0).is_empty(),
            "a set that has to hold two 16 MiB pages at once was let through a 32 MiB budget"
        );
        assert_eq!(decoder.budget_resets(), 1);
        assert_eq!(decoder.take_decode_errors(), 1);

        // The same picture WITHOUT a crop fits, which is what makes the case
        // above about the copy rather than about the picture.
        let mut decoder = PgsDecoder::new();
        decoder.set_video_size(1920, 1080);
        let uncropped = joined(&[
            presentation((1920, 1080), &[composed(1, 0, 0)]),
            palette(&[(1, 235, 128, 128, 255)]),
            object_first(1, 0, data.len(), &data),
            end(),
        ]);
        assert_eq!(decode_one(&mut decoder, &uncropped, 0).len(), 1);
        assert_eq!(decoder.budget_resets(), 0);
    }

    /// A display set that composes only objects this decoder has never
    /// seen is a mid-stream JOIN, not a corrupt stream.
    ///
    /// The shape is ordinary: a seek lands inside an epoch, and the next
    /// display set is a palette-only update naming pictures whose object
    /// segments went past before the seek. Counting that as malformed wiped the
    /// palette and the object store and cascaded into every set behind it,
    /// exactly the recovery the error policy exists to provide, inverted.
    #[test]
    fn a_set_composing_only_unseen_objects_is_a_join_not_a_fault() {
        gst::init().unwrap();
        let mut decoder = PgsDecoder::new();
        decoder.set_video_size(1920, 1080);

        // Land mid-epoch: a set naming objects whose pictures never arrived.
        let palette_only = joined(&[
            presentation(
                (1920, 1080),
                &[composed(7, 100, 200), composed(8, 300, 200)],
            ),
            palette(&[(1, 235, 128, 128, 255)]),
            end(),
        ]);
        let updates = decode_one(&mut decoder, &palette_only, 1_000);
        assert!(
            updates.is_empty(),
            "a set with no pictures behind it must show nothing, not clear the screen"
        );
        assert_eq!(
            decoder.take_decode_errors(),
            0,
            "the normal shape of a mid-stream join was counted as a corrupt stream"
        );

        // And the epoch that follows decodes, which is what the cascade broke.
        assert_eq!(
            decode_one(&mut decoder, &minimal_display_set(), 2_000).len(),
            1
        );
        assert_eq!(decoder.take_decode_errors(), 0);
    }

    /// An object segment for an id the open composition does not name is
    /// refused, which is both the reference's rule and the end of a budget
    /// thrash: a stream that fills the store with pictures no set will draw
    /// leaves no budget for the ones it will.
    #[test]
    fn an_object_the_composition_does_not_name_is_refused() {
        gst::init().unwrap();
        let mut decoder = PgsDecoder::new();
        decoder.set_video_size(1920, 1080);

        let data = picture_data(4, 2, &[&[(1, 4)], &[(1, 4)]]);
        let mut bytes = presentation((1920, 1080), &[composed(1, 0, 0)]);
        bytes.extend_from_slice(&palette(&[(1, 235, 128, 128, 255)]));
        bytes.extend_from_slice(&object_first(1, 0, data.len(), &data));
        // Two more objects, unnamed by the composition, each declaring a
        // sixteenth of the budget.
        for id in 2..=3u16 {
            bytes.extend_from_slice(&object_first(id, 0, 2 * 1024 * 1024, &[0, 4, 0, 4]));
        }
        bytes.extend_from_slice(&end());

        let updates = decode_one(&mut decoder, &bytes, 0);
        assert_eq!(updates.len(), 1, "the set the composition did name");
        assert_eq!(
            decoder.held_bytes(),
            data.len() as u64,
            "the store took objects no composition names"
        );
        assert_eq!(
            decoder.take_decode_errors(),
            0,
            "refusing them is not a fault"
        );
    }

    /// A region's rect is CONTAINED in the picture, not merely started
    /// inside it.
    ///
    /// A one-pixel canvas is well-formed and costs two bytes to write; scaled
    /// onto a full-HD frame it turns a four-pixel-wide picture into a
    /// 4320-wide rect. Nothing downstream wants that, and where it is clipped
    /// should be here.
    #[test]
    fn a_region_rect_is_contained_in_the_picture() {
        gst::init().unwrap();
        let mut decoder = PgsDecoder::new();
        decoder.set_video_size(1920, 1080);

        let data = picture_data(4, 2, &[&[(1, 4)], &[(1, 4)]]);
        let bytes = joined(&[
            presentation((1, 1), &[composed(1, 0, 0)]),
            palette(&[(1, 235, 128, 128, 255)]),
            object_first(1, 0, data.len(), &data),
            end(),
        ]);

        let updates = decode_one(&mut decoder, &bytes, 0);
        let region = &updates[0].regions[0];
        assert!(
            region.x >= 0 && region.y >= 0,
            "a region placed off the top-left: {},{}",
            region.x,
            region.y
        );
        assert!(
            region.x as u32 + region.render_width <= 1920
                && region.y as u32 + region.render_height <= 1080,
            "the rect runs off the picture: {},{} {}x{}",
            region.x,
            region.y,
            region.render_width,
            region.render_height
        );
    }

    /// The object store follows the composition: an object no display set names
    /// any more is not kept.
    ///
    /// Objects deliberately SURVIVE a display set, because a palette-only
    /// update names a picture it does not re-send. The limit on that is this:
    /// the moment a composition stops naming an object, the picture goes.
    /// Without it a stream can park the decoder's whole allocation budget in
    /// objects nothing will ever draw, and every later display set is refused
    /// by the cap, so subtitles are off for the rest of the file with the cap
    /// working exactly as designed. Held bytes are the observable.
    #[test]
    fn the_object_store_follows_the_composition() {
        gst::init().unwrap();
        let mut decoder = PgsDecoder::new();
        decoder.set_video_size(1920, 1080);

        // A set naming two objects: one whole, one whose declared length is a
        // megabyte the stream never sends. The set still shows (the whole
        // object is a region), and the decoder is committed to both lengths.
        let data = picture_data(4, 2, &[&[(1, 4)], &[(1, 4)]]);
        let first = joined(&[
            presentation((1920, 1080), &[composed(1, 0, 0), composed(2, 40, 0)]),
            palette(&[(1, 235, 128, 128, 255)]),
            object_first(1, 0, data.len(), &data),
            object_first(2, 0, 1_000_000, &data),
            end(),
        ]);
        assert_eq!(decode_one(&mut decoder, &first, 0).len(), 1);
        assert_eq!(
            decoder.held_bytes(),
            data.len() as u64 + 1_000_000,
            "the objects' declared lengths are what the decoder is committed to"
        );

        // The next set names object 1 alone. Object 2 is now a picture nothing
        // refers to.
        let second = joined(&[
            presentation((1920, 1080), &[composed(1, 0, 0)]),
            palette(&[(1, 235, 128, 128, 255)]),
            object_first(1, 0, data.len(), &data),
            end(),
        ]);
        assert_eq!(decode_one(&mut decoder, &second, 1_000).len(), 1);
        assert_eq!(
            decoder.held_bytes(),
            data.len() as u64,
            "an object no composition names any more is still being held"
        );
    }

    // -------------------------------------------------- the engine, for real

    /// THE RENDER PROOF: hand-crafted PGS bytes through the ENGINE's production
    /// wiring (no test decoder installed, so the decoder under this is the one
    /// [`crate::subpic::decoder_for`] builds) and out the other side as a
    /// source-frame overlay with the decoder's own pixels in it.
    ///
    /// This is also the flip's proof at the engine's edge: before PGS landed,
    /// `decoder_for` answered `None` here and every packet was a counted decode
    /// error.
    #[test]
    fn a_display_set_reaches_the_overlay_set_through_the_production_decoder() {
        gst::init().unwrap();
        let engine = CueEngine::new();
        engine.set_video_size(1920, 1080);

        engine.submit_bitmap(packet(&minimal_display_set(), 1_000));

        let at = gst::ClockTime::from_mseconds(1_200);
        assert!(
            wait_for(|| !engine.overlays_for(Some(at)).is_empty()),
            "the decoded set never reached the overlay set"
        );

        let overlays = engine.overlays_for(Some(at));
        assert_eq!(overlays.len(), 1);
        assert_eq!(
            overlays[0].space,
            OverlaySpace::SrcFrame,
            "a bitmap subtitle belongs to the picture, not to the window"
        );
        assert_eq!((overlays[0].width, overlays[0].height), (4, 2));
        assert_eq!(
            (overlays[0].x, overlays[0].y),
            (100, 200),
            "the coded size the engine taught reached the decoder's geometry"
        );
        assert_eq!(
            &overlays[0].pixels[0..4],
            &[255, 24, 0, 255],
            "these are the decoder's own pixels"
        );
        assert_eq!(engine.bitmap_sets_decoded(), 1);
        assert_eq!(
            engine.bitmap_decode_errors(),
            0,
            "a well-formed display set cost the engine an error"
        );

        // The stream's own clear, through the same path: a set with no
        // composition takes the picture off the screen at its running time.
        engine.submit_bitmap(packet(
            &joined(&[presentation((1920, 1080), &[]), end()]),
            2_000,
        ));
        let after = gst::ClockTime::from_mseconds(2_100);
        assert!(
            wait_for(|| engine.overlays_for(Some(after)).is_empty()),
            "the scheduled clear never took the set off the screen"
        );
        assert_eq!(engine.bitmap_decode_errors(), 0);
        assert_eq!(engine.bitmap_overflow_resets(), 0);
        assert_eq!(engine.bitmap_dropped_sets(), 0);
    }

    /// A malformed stream costs the ENGINE a counted decode error, which is the
    /// other end of the decoder's own counter: the cap asks for the count, and
    /// this is where it lands.
    #[test]
    fn a_malformed_set_counts_a_decode_error_at_the_engine() {
        gst::init().unwrap();
        let engine = CueEngine::new();
        engine.set_video_size(1920, 1080);

        engine.submit_bitmap(packet(
            &joined(&[segment(SEGMENT_PRESENTATION, &[0, 0, 0, 0]), end()]),
            1_000,
        ));
        assert!(
            wait_for(|| engine.bitmap_decode_errors() == 1),
            "the decoder's counted reset never reached the engine's counter"
        );

        // And the next good set still shows: a reset recovers, it does not
        // switch subtitles off.
        engine.submit_bitmap(packet(&minimal_display_set(), 2_000));
        let at = gst::ClockTime::from_mseconds(2_100);
        assert!(
            wait_for(|| !engine.overlays_for(Some(at)).is_empty()),
            "the decoder never recovered from a malformed set"
        );
    }
}
