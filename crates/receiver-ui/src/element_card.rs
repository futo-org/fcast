//! The inspector's stream card, built from the element's stats.
//!
//! The old lane assembled this on its streaming thread because it had the caps
//! plan in hand. The element already describes its stream in `stats` and says
//! when that description moved, so this reads it on the notification and never
//! per frame.

use slint::{ComponentHandle, ToSharedString};

/// Reads the element's description and puts it on the bridge. UI thread.
pub(crate) fn publish(ui: &crate::MainWindow, stats: &gst::Structure) {
    let Some(card) = card(stats) else {
        // Before the first caps there is no stream to describe, and a stale
        // card is worse than none.
        ui.global::<crate::Bridge>().set_have_video_dbg_info(false);
        return;
    };
    let bridge = ui.global::<crate::Bridge>();
    bridge.set_video_frame_width(card.width);
    bridge.set_video_frame_height(card.height);
    bridge.set_video_dbg_info(card.info);
    bridge.set_have_video_dbg_info(true);
}

struct Card {
    info: crate::UiVideoDbgInfo,
    width: i32,
    height: i32,
}

/// `None` until the element has seen caps, which is what leaves out the size.
fn card(stats: &gst::Structure) -> Option<Card> {
    let width: u32 = stats.get("width").ok()?;
    let height: u32 = stats.get("height").ok()?;
    let text = |key| stats.get::<String>(key).unwrap_or_default();

    let framerate = match stats.get::<f64>("framerate").unwrap_or(0.0) {
        fps if fps > 0.0 => format!("{fps:.3} fps"),
        _ => String::new(),
    };
    let hdr = if stats.get::<bool>("hdr").unwrap_or(false) {
        format!(
            "{}, mastering {:.0} nits, MaxCLL {:.0}",
            text("transfer"),
            stats.get::<f32>("max-mastering-nits").unwrap_or(0.0),
            stats.get::<f32>("max-cll").unwrap_or(0.0),
        )
    } else {
        "SDR".to_owned()
    };

    Some(Card {
        width: width as i32,
        height: height as i32,
        info: crate::UiVideoDbgInfo {
            format: format!(
                "{} ({}-bit)",
                text("format"),
                stats.get::<u32>("coded-bits").unwrap_or(0)
            )
            .to_shared_string(),
            resolution: format!("{width}x{height}").to_shared_string(),
            framerate: framerate.to_shared_string(),
            pixel_aspect: text("pixel-aspect-ratio").to_shared_string(),
            rotation: rotation(stats).to_shared_string(),
            memory: memory(stats).to_shared_string(),
            primaries: text("primaries").to_shared_string(),
            transfer: text("transfer").to_shared_string(),
            matrix: text("matrix").to_shared_string(),
            range: text("range").to_shared_string(),
            hdr: hdr.to_shared_string(),
        },
    })
}

/// The turn in degrees, and the mirror when there is one. The degrees alone
/// would call a flipped picture upright.
fn rotation(stats: &gst::Structure) -> String {
    let degrees = stats.get::<u32>("rotation-degrees").unwrap_or(0);
    let flipped = stats
        .get::<String>("transform")
        .map(|t| t.starts_with("Flipped"))
        .unwrap_or(false);
    match flipped {
        true => format!("flipped {degrees}°"),
        false => format!("{degrees}°"),
    }
}

/// What route the last frame took, in the words the card uses.
fn memory(stats: &gst::Structure) -> &'static str {
    match stats.get::<String>("arm").unwrap_or_default().as_str() {
        "import" => "zero-copy import",
        "upload" => "upload",
        _ => "none yet",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // Only the fixtures below build a `SendValue`; the card itself reads the
    // structure through inherent methods.
    use gst::prelude::*;

    fn stats(fields: &[(&str, gst::glib::SendValue)]) -> gst::Structure {
        // A structure is a GObject type, so the registry has to exist first.
        gst::init().expect("gstreamer initialises");
        let mut s = gst::Structure::new_empty("slintvideosink/stats");
        for (name, value) in fields {
            s.set_value(*name, value.clone());
        }
        s
    }

    /// Before the first caps there is no size, and a card without one would
    /// show the inspector a stream that does not exist.
    #[test]
    fn a_description_without_a_size_is_no_card() {
        assert!(card(&stats(&[("arm", "none".to_send_value())])).is_none());
    }

    /// Everything the card shows comes off one structure, so a missing field
    /// has to leave a blank rather than take the card down.
    #[test]
    fn a_sparse_description_still_builds() {
        let built = card(&stats(&[
            ("width", 1920u32.to_send_value()),
            ("height", 1080u32.to_send_value()),
        ]))
        .expect("a size is all the card needs");
        assert_eq!(built.width, 1920);
        assert_eq!(built.height, 1080);
        assert_eq!(built.info.resolution, "1920x1080");
        assert_eq!(built.info.framerate, "", "no framerate is blank, not zero fps");
        assert_eq!(built.info.hdr, "SDR");
        assert_eq!(built.info.memory, "none yet");
    }

    /// A flip is not a rotation and the card must not report it as upright.
    #[test]
    fn a_mirrored_picture_says_so() {
        let upright = stats(&[
            ("rotation-degrees", 0u32.to_send_value()),
            ("transform", "Normal".to_send_value()),
        ]);
        let mirrored = stats(&[
            ("rotation-degrees", 180u32.to_send_value()),
            ("transform", "Flipped180".to_send_value()),
        ]);
        assert_eq!(rotation(&upright), "0°");
        assert_eq!(rotation(&mirrored), "flipped 180°");
    }

    /// The HDR line carries the mastering numbers, and an SDR stream says so
    /// in one word rather than printing zeroes.
    #[test]
    fn hdr_reads_as_its_numbers_and_sdr_as_itself() {
        let sdr = card(&stats(&[
            ("width", 1280u32.to_send_value()),
            ("height", 720u32.to_send_value()),
            ("hdr", false.to_send_value()),
        ]))
        .unwrap();
        assert_eq!(sdr.info.hdr, "SDR");

        let hdr = card(&stats(&[
            ("width", 3840u32.to_send_value()),
            ("height", 2160u32.to_send_value()),
            ("hdr", true.to_send_value()),
            ("transfer", "Pq".to_send_value()),
            ("max-mastering-nits", 1000.0f32.to_send_value()),
            ("max-cll", 820.0f32.to_send_value()),
        ]))
        .unwrap();
        assert_eq!(hdr.info.hdr, "Pq, mastering 1000 nits, MaxCLL 820");
    }

    /// The route is the number the card leads with, so the two arms have to
    /// read differently.
    #[test]
    fn the_route_is_named() {
        assert_eq!(memory(&stats(&[("arm", "import".to_send_value())])), "zero-copy import");
        assert_eq!(memory(&stats(&[("arm", "upload".to_send_value())])), "upload");
        assert_eq!(memory(&stats(&[])), "none yet");
    }
}
