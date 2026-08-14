//! Bitmap ("subpicture") subtitles: the types the sink-side engine schedules
//! and the seam the per-format decoders plug into.
//!
//! Bitmap subtitles arrive as compressed pictures with their own palettes,
//! display geometry, and (for some formats) stateful reassembly across several
//! buffers. A text cue is schedulable the moment it arrives; a bitmap set is
//! the output of a decoder that must see the packets in order, which is why
//! this module lives beside [`crate::cue`] rather than inside it.
//!
//! Division of work:
//!
//!  * the **driver** (`fcastplaybin`) stays a byte pipe. It decides from caps
//!    that a stream is a bitmap format, converts the sample's pts to running
//!    time, and hands the untouched [`gst::Buffer`] over as a [`BitmapPacket`].
//!  * the **engine** owns reassembly and decode on its own worker thread, so
//!    nothing here ever runs on a streaming thread.
//!  * a **decoder** ([`SubpicDecoder`]) is a pure state machine from packets to
//!    [`DisplayUpdate`]s, with no GStreamer types beyond the bytes it maps, so
//!    unit tests can drive it without a pipeline.
//!
//! All three formats (PGS, VOBSUB, DVB) are implemented and admitted by the
//! driver's caps gate.

pub mod dvb;
pub mod pgs;
pub mod vobsub;

use std::sync::Arc;

use crate::video::{Overlay, OverlaySpace};

/// The bitmap subtitle formats the engine can be fed.
///
/// Mirrors the driver's `BitmapSubFormat` as its own type because this crate
/// cannot depend on the driver (the dependency runs the other way). The
/// conversion is one match arm in the receiver's glue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BitmapFormat {
    /// Blu-ray Presentation Graphic Stream (`subpicture/x-pgs`).
    Pgs,
    /// DVD subpicture units (`subpicture/x-dvd`), palette out of band.
    Vobsub,
    /// DVB subtitles, ETSI EN 300 743 (`subpicture/x-dvb`).
    Dvb,
}

impl BitmapFormat {
    /// Every format in this enum, in one place, so tests that must cover the
    /// whole enum have a single list. Adding a variant without adding it here
    /// fails to compile, because the length is part of the type.
    pub const ALL: [BitmapFormat; 3] = [BitmapFormat::Pgs, BitmapFormat::Vobsub, BitmapFormat::Dvb];
}

/// One appsink sample's worth of a bitmap subtitle stream, exactly as the
/// driver saw it.
///
/// The buffer rides by reference-count. No map, copy, or validation happens on
/// the delivery thread; mapping is the decoder's job, on the engine's thread.
///
/// `rt` is running time on the VIDEO base (the driver applies the text pad's
/// sync offset before converting), which is the same clock every [`crate::cue`]
/// cue is scheduled in.
///
/// **No `PartialEq`, deliberately.** gstreamer-rs implements buffer equality
/// as a content comparison, and subtitle packets routinely carry identical
/// bytes, so a derived `==` would conflate distinct packets. The engine's
/// de-duplication needs object identity and spells it out (`cue::same_buffer`).
#[derive(Debug, Clone)]
pub struct BitmapPacket {
    pub format: BitmapFormat,
    pub data: gst::Buffer,
    /// Out-of-band setup bytes from the caps (`codec_data`). VOBSUB's palette
    /// travels here; the other formats leave it `None`.
    pub codec_data: Option<gst::Buffer>,
    pub rt: gst::ClockTime,
    /// The buffer's duration when the container gives one (mkv BlockDuration);
    /// the decoders use it as the fallback end for formats whose in-band stop
    /// time may be missing.
    pub duration: Option<gst::ClockTime>,
}

/// One decoded picture, already in the shape the compositor wants.
///
/// RGBA8, tightly packed, **straight (non-premultiplied) alpha**. The renderer
/// composites with `PL_ALPHA_INDEPENDENT`, so a decoder that premultiplies
/// would wash every semi-transparent pixel out.
///
/// `x`/`y`/`render_width`/`render_height` are in VIDEO (coded) pixels, not
/// window pixels: the regions are laid out against the picture and scale and
/// rotate with it, which is what [`OverlaySpace::SrcFrame`] means. Decoders do
/// that scaling at decode time, from their format's own display grid to the
/// size the engine last learned through
/// [`crate::cue::CueEngine::set_video_size`], and leave the texture at its
/// native size so the renderer's single resample does the rest.
#[derive(Debug, Clone)]
pub struct BitmapRegion {
    /// Refcount-shared with every [`Overlay`] built from this region: a
    /// full-page subtitle is megabytes and an overlay set is built per
    /// displayed frame, so it is cloned by pointer, never memcpy'd.
    pub pixels: Arc<Vec<u8>>,
    /// Texture dimensions of `pixels` (`width * height * 4` bytes).
    pub width: u32,
    pub height: u32,
    /// Render rectangle, in video pixels.
    pub x: i32,
    pub y: i32,
    pub render_width: u32,
    pub render_height: u32,
}

impl BitmapRegion {
    /// What this region costs in pixel memory, for the engine's pending-store
    /// budget.
    pub fn pixel_bytes(&self) -> usize {
        self.pixels.len()
    }

    /// The overlay this region renders as. Cheap: the pixels are shared by
    /// pointer, so this is a handful of scalar copies per frame.
    pub(crate) fn to_overlay(&self) -> Overlay {
        Overlay {
            pixels: self.pixels.clone(),
            width: self.width,
            height: self.height,
            x: self.x,
            y: self.y,
            render_width: self.render_width,
            render_height: self.render_height,
            // Source-frame space: unlike a text cue, which is laid out at
            // display resolution and must stay upright and unscaled, a bitmap
            // subtitle was authored against the picture and belongs to it.
            space: OverlaySpace::SrcFrame,
        }
    }
}

/// Everything a decoder has to say about what should be on screen, from
/// `start_rt` until something replaces it.
///
/// One update is a complete description, never a delta. Any incremental
/// painting a format does has already happened inside the decoder, which lets
/// the engine treat a newer update as whole-set replacement and lets its
/// pending store trim rather than reset.
///
/// Three shapes, and the engine distinguishes exactly these:
///
///  * **regions, no end**: show until superseded (PGS's normal case);
///  * **regions, an end**: show until superseded or until `end_rt`, whichever
///    comes first;
///  * **no regions**: a *scheduled clear*. At `start_rt`, whatever is showing
///    stops showing. Deliberately not the same thing as
///    [`crate::cue::CueEngine::clear`], the immediate track-switch primitive.
#[derive(Debug, Clone)]
pub struct DisplayUpdate {
    pub start_rt: gst::ClockTime,
    /// `None` means open-ended. Only supersession or a clear takes it away.
    pub end_rt: Option<gst::ClockTime>,
    /// Empty means "show nothing from `start_rt`". See the type docs.
    pub regions: Vec<BitmapRegion>,
}

impl DisplayUpdate {
    /// What this update's regions cost in pixel memory, added up region by
    /// region.
    ///
    /// A diagnostic, not the budget. The engine's pending-store budget accounts
    /// by allocation (every distinct `Arc` once across the whole store), so a
    /// decoder that shares one page between many updates is charged once. This
    /// sum double-counts such sharing and is kept for tests and logs.
    pub fn pixel_bytes(&self) -> usize {
        self.regions.iter().map(BitmapRegion::pixel_bytes).sum()
    }
}

/// A per-format bitmap subtitle decoder: packets in, display updates out.
///
/// Implementations are pure state machines. They are built, driven and dropped
/// on the engine's `fvid-sub-decode` thread and never touch engine state, which
/// is what keeps the threading story to "one worker, one lock pair".
///
/// The contract the engine relies on:
///
///  * `push` NEVER panics on malformed input. Stream bytes are untrusted, so
///    malformed data is a counted, logged reset inside the decoder, not an
///    assertion. (The engine catches a panic anyway and rebuilds the decoder,
///    but that is a backstop for programmer error, not the policy.)
///  * `reset` returns the decoder to its just-constructed state, dropping any
///    partial reassembly. The engine calls it whenever the epoch changes (a
///    flush, a clear, a new stream), because accumulated state then describes
///    the wrong timeline.
///  * `push` may answer with zero, one or several updates: a packet that only
///    carries a fragment produces none, and a packet that closes a set whose
///    predecessor also timed out can produce two.
pub trait SubpicDecoder {
    /// Out-of-band setup bytes from the caps. Called before `push` whenever the
    /// packet's `codec_data` differs from what was last applied.
    fn set_codec_data(&mut self, data: &[u8]);

    /// The coded video size regions must be scaled to. Zero dimensions are
    /// never passed in. The engine drops those at its own door.
    fn set_video_size(&mut self, width: u32, height: u32);

    /// Feed one packet.
    fn push(&mut self, packet: &BitmapPacket) -> Vec<DisplayUpdate>;

    /// Drop all accumulated state.
    fn reset(&mut self);

    /// How many display sets the decoder has thrown away since this was last
    /// asked, and zero for a decoder that never throws anything away.
    ///
    /// The engine drains this after every `push` and adds it to
    /// `bitmap_decode_errors`. It is a counter, not an error return: a decoder
    /// that dropped one set and produced another from the same packet has done
    /// both, and the engine wants both facts.
    fn take_decode_errors(&mut self) -> u64 {
        0
    }
}

/// Whether a decoder for this format exists.
///
/// **The one place the implemented set is written down.** [`decoder_for`]
/// derives from it. The driver's caps gate keeps a mirror of the same set
/// (the driver cannot see this crate), and the receiver, which depends on
/// both, asserts the two agree. A format that gets past the gate with no
/// decoder here is a bug, and the engine counts it as a decode error.
pub fn implemented(format: BitmapFormat) -> bool {
    match format {
        BitmapFormat::Pgs | BitmapFormat::Vobsub | BitmapFormat::Dvb => true,
    }
}

/// The decoder for a format, or `None` when the format has none yet.
///
/// Derived from [`implemented`] rather than deciding for itself, so there is
/// one set and one place to edit.
pub fn decoder_for(format: BitmapFormat) -> Option<Box<dyn SubpicDecoder>> {
    if !implemented(format) {
        return None;
    }
    match format {
        BitmapFormat::Pgs => Some(Box::new(pgs::PgsDecoder::new())),
        BitmapFormat::Vobsub => Some(Box::new(vobsub::VobsubDecoder::new())),
        BitmapFormat::Dvb => Some(Box::new(dvb::DvbDecoder::new())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The implemented set and the decoder table are the same claim said
    /// twice; this catches a format landing in one without the other.
    #[test]
    fn the_implemented_set_and_the_decoder_table_agree() {
        for format in BitmapFormat::ALL {
            assert_eq!(
                implemented(format),
                decoder_for(format).is_some(),
                "{format:?}: `implemented` and `decoder_for` disagree"
            );
            // The agreement above is satisfied by a format absent from both
            // sides; this asserts every format is actually in.
            assert!(
                implemented(format),
                "{format:?} has no decoder; every format in this enum is supposed to have one \
                 to have one, and a new format needs its own decoder rather than a \
                 quiet arm here"
            );
        }
    }
}
