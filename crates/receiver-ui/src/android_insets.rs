//! Window inset math for the android overlays, host-testable. All values are
//! physical px in the window root view's space, the one the root insets and
//! the content frame's location are reported in.

/// The part of the root's bottom inset that actually covers the content
/// frame. API 28/29 still fit the decor to system windows, so the frame
/// already ends above the nav bar (and a resize keyboard) and the raw inset
/// would lift cues by it twice. Edge-to-edge (30+) the frame runs to the
/// bottom and the whole inset overlaps.
///
/// `root_h` 0 means unknown and yields the raw inset, the edge-to-edge answer.
/// A frame panned past the root bottom still caps at the inset, and the
/// result never exceeds the frame itself.
pub(crate) fn bottom_overlap(root_h: i32, content_y: i32, content_h: u32, inset: u32) -> u32 {
    let bottom = content_y as i64 + content_h as i64;
    let below = (root_h as i64 - bottom).max(0);
    (inset as i64 - below).clamp(0, content_h as i64) as u32
}

#[cfg(test)]
mod tests {
    use super::bottom_overlap;

    // 1080x2340 portrait phone: 63px status bar, 126px nav bar, 800px IME
    const ROOT: i32 = 2340;
    const STATUS: i32 = 63;
    const NAV: u32 = 126;
    const IME: u32 = 800;

    #[test]
    fn api28_fitted_frame_already_clears_the_nav_bar() {
        let h = (ROOT - STATUS) as u32 - NAV;
        assert_eq!(bottom_overlap(ROOT, STATUS, h, NAV), 0);
    }

    #[test]
    fn api30_edge_to_edge_overlaps_the_whole_inset() {
        assert_eq!(bottom_overlap(ROOT, 0, ROOT as u32, NAV), NAV);
    }

    #[test]
    fn immersive_has_no_inset() {
        assert_eq!(bottom_overlap(ROOT, 0, ROOT as u32, 0), 0);
    }

    #[test]
    fn api28_immersive_stable_inset_over_fitted_frame() {
        // LAYOUT_STABLE keeps reporting the hidden bar, the frame stays fitted
        let h = (ROOT - STATUS) as u32 - NAV;
        assert_eq!(bottom_overlap(ROOT, STATUS, h, NAV), 0);
    }

    #[test]
    fn api28_resize_keyboard_shrinks_the_frame_with_the_inset() {
        let inset = NAV + IME;
        let h = (ROOT - STATUS) as u32 - inset;
        assert_eq!(bottom_overlap(ROOT, STATUS, h, inset), 0);
    }

    #[test]
    fn edge_to_edge_keyboard_overlaps_bar_and_ime() {
        let inset = NAV + IME;
        assert_eq!(bottom_overlap(ROOT, 0, ROOT as u32, inset), inset);
    }

    #[test]
    fn partial_overlap_counts_only_the_covered_rows() {
        // frame ends 40px above the root bottom
        let h = (ROOT - 40) as u32;
        assert_eq!(bottom_overlap(ROOT, 0, h, NAV), NAV - 40);
    }

    #[test]
    fn unknown_root_falls_back_to_the_raw_inset() {
        assert_eq!(bottom_overlap(0, STATUS, 2000, NAV), NAV);
    }

    #[test]
    fn panned_frame_caps_at_the_inset() {
        // runs 300px past the root bottom
        assert_eq!(bottom_overlap(ROOT, 300, ROOT as u32, NAV), NAV);
        // shifted up clear of the inset
        assert_eq!(bottom_overlap(ROOT, -300, ROOT as u32, NAV), 0);
    }

    #[test]
    fn overlap_never_exceeds_the_frame() {
        assert_eq!(bottom_overlap(ROOT, 0, 100, 0), 0);
        assert_eq!(bottom_overlap(0, 0, 100, 500), 100);
    }

    #[test]
    fn extreme_values_do_not_wrap() {
        assert_eq!(
            bottom_overlap(i32::MAX, i32::MIN, u32::MAX, u32::MAX),
            u32::MAX
        );
        assert_eq!(bottom_overlap(i32::MAX, i32::MIN, 0, u32::MAX), 0);
        assert_eq!(bottom_overlap(i32::MIN, i32::MAX, 10, u32::MAX), 10);
    }
}
