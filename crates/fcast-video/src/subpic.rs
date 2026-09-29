//! Bitmap subtitles as this crate needs them: the driver's decoders, plus the
//! one conversion that is ours.
//!
//! The decoders moved to [`flapjack::subpic`]. They consume packets off the
//! driver's own subtitle pad and have no renderer in them, so keeping a copy
//! per consumer meant two crates each mirroring the other's implemented-set
//! and a second spelling of the format enum. What stays here is the step the
//! driver deliberately has no opinion about: turning a decoded region into the
//! overlay an Android lane draws.

pub use flapjack::subpic::{
    BitmapPacket, BitmapRegion, BitmapSubFormat, DisplayUpdate, SubpicDecoder, decoder_for,
    dvb, implemented, pgs, vobsub,
};

use crate::video::{Overlay, OverlaySpace};

/// The overlay a decoded region renders as. Cheap: the pixels are shared by
/// pointer, so this is a handful of scalar copies per frame.
///
/// A free function rather than a method, because the region is the driver's
/// type and the overlay is this crate's.
pub(crate) fn region_overlay(region: &BitmapRegion) -> Overlay {
    Overlay {
        pixels: region.pixels.clone(),
        width: region.width,
        height: region.height,
        x: region.x,
        y: region.y,
        render_width: region.render_width,
        render_height: region.render_height,
        // Source-frame space: unlike a text cue, which is laid out at display
        // resolution and must stay upright and unscaled, a bitmap subtitle was
        // authored against the picture and belongs to it.
        space: OverlaySpace::SrcFrame,
    }
}
