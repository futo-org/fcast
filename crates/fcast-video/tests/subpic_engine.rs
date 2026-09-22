//! The bitmap decoders through the ENGINE's production wiring.
//!
//! The decoders themselves are `flapjack::subpic`'s and are unit tested there,
//! against bytes rather than against a schedule. What is here is the other
//! half: a real display set from the driver's fixtures, through
//! [`CueEngine`]'s own decoder factory, out as an overlay, and off the screen
//! again by whichever rule that format expires under. It lives in this crate
//! because the engine does.
//!
//! Every one of these ran inside the decoder modules before the decoders
//! moved. The fixtures they are built from are the same ones, now public
//! behind `#[doc(hidden)]`.

use std::sync::Arc;

use fcast_video::{cue::CueEngine, video::OverlaySpace};
use flapjack::subpic::{BitmapPacket, BitmapSubFormat};

/// The engine's worker decodes off-thread, so every assertion about what
/// reached the overlay set has to wait for it rather than read once.
fn wait_for(condition: impl Fn() -> bool) -> bool {
    for _ in 0..200 {
        if condition() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    condition()
}

fn packet_for(format: BitmapSubFormat, bytes: &[u8], rt_ms: u64) -> BitmapPacket {
    BitmapPacket {
        format,
        data: gst::Buffer::from_slice(bytes.to_vec()),
        codec_data: None,
        rt: gst::ClockTime::from_mseconds(rt_ms),
        duration: None,
    }
}


mod dvb_engine {
    use super::*;
    use flapjack::subpic::dvb::fixtures::*;

    /// THE RENDER PROOF: a hand-built display set through the ENGINE's
    /// production wiring, and out as a source-frame overlay whose own timeout
    /// takes it away again.
    #[test]
    fn a_display_set_reaches_the_overlay_set_and_times_out() {
        gst::init().unwrap();
        let engine = CueEngine::new();
        engine.set_video_size(720, 576);

        engine.submit_bitmap(packet_for(BitmapSubFormat::Dvb, &minimal_display_set(), 1_000));
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

mod pgs_engine {
    use super::*;
    use flapjack::subpic::pgs::fixtures::*;

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

        engine.submit_bitmap(packet_for(BitmapSubFormat::Pgs, &minimal_display_set(), 1_000));

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
        engine.submit_bitmap(packet_for(
            BitmapSubFormat::Pgs,
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

        // 0x16 is the presentation segment, truncated to four bytes.
        engine.submit_bitmap(packet_for(
            BitmapSubFormat::Pgs,
            &joined(&[segment(0x16, &[0, 0, 0, 0]), end()]),
            1_000,
        ));
        assert!(
            wait_for(|| engine.bitmap_decode_errors() == 1),
            "the decoder's counted reset never reached the engine's counter"
        );

        // And the next good set still shows: a reset recovers, it does not
        // switch subtitles off.
        engine.submit_bitmap(packet_for(BitmapSubFormat::Pgs, &minimal_display_set(), 2_000));
        let at = gst::ClockTime::from_mseconds(2_100);
        assert!(
            wait_for(|| !engine.overlays_for(Some(at)).is_empty()),
            "the decoder never recovered from a malformed set"
        );
    }
}

mod vobsub_engine {
    use super::*;
    use flapjack::subpic::vobsub::fixtures::*;

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
        let mut packet = packet_for(BitmapSubFormat::Vobsub, &minimal_unit(), frozen.mseconds());
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

        let mut packet = packet_for(BitmapSubFormat::Vobsub, &minimal_unit(), 1_000);
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
