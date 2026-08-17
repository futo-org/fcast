//! Resolution-independent UI scaling policy.
//!
//! The GUI is authored in fixed `px` against one logical canvas
//! ([`DESIGN_WIDTH`] x [`DESIGN_HEIGHT`]). Nothing in the .slint files sizes
//! itself off the window; the whole scene is zoomed through the window's scale
//! factor instead, so every resolution renders the same layout at a different
//! pixel density.
//!
//! TV mode is the default ([`DEFAULT_MODE`]): it ignores the DPI scale and fits
//! the design canvas to the window, which is what a 10-foot UI needs, because a
//! 4K TV usually reports no DPI scale at all and a desktop-scaled UI then lands
//! at half the physical size it has on a 1080p set. Desktop mode is the opt-out
//! that keeps whatever factor the windowing system reports.
//!
//! This module is pure policy plus the panel-size probe. Applying the resolved
//! factor to a live window is the GUI's job (see `receiver-ui`'s `scaling`).

#[cfg(target_os = "linux")]
use tracing::{debug, warn};

/// Logical canvas the .slint files are authored against.
pub const DESIGN_WIDTH: f32 = 1280.0;
/// Logical canvas the .slint files are authored against.
pub const DESIGN_HEIGHT: f32 = 720.0;

/// Resolved factors snap to this step. Arbitrary fractions lay out fine but
/// cost glyph sharpness for no visible gain.
const SNAP: f32 = 0.125;
const MIN_SCALE: f32 = 0.5;
const MAX_SCALE: f32 = 6.0;

/// Diagonal from which a panel counts as a TV rather than a desk monitor.
const TV_DIAGONAL_INCHES: f32 = 30.0;

/// Below this the fit is 1.0 anyway, so a false TV verdict costs nothing;
/// above it, guessing wrong is very visible.
const TV_MIN_HEIGHT: u32 = 1000;

/// A system scale above this means the platform already sized for the panel.
const TV_MAX_SYSTEM_SCALE: f32 = 1.05;

/// How the window scale factor is chosen. `[interface] ui_scale` /
/// `--ui-scale`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum UiScale {
    /// TV policy on a panel that looks like a TV, DPI scale otherwise.
    Auto,
    /// Whatever the windowing system reports.
    Desktop,
    /// Fit the design canvas to the window.
    Tv,
    /// A literal factor.
    Fixed(f32),
}

/// What an unset `ui_scale` means. The receiver is a casting target first, so
/// it is sized for the couch by default and never for reading distance; the DPI
/// floor in [`tv_scale`] keeps that safe on desk monitors too.
pub const DEFAULT_MODE: UiScale = UiScale::Tv;

/// How [`DEFAULT_MODE`] spells itself, for the settings drawer's dropdown. The
/// drawer shows the mode by name rather than a "Default" sentinel, so the couch
/// default is visible instead of implied.
pub const DEFAULT_MODE_NAME: &str = "tv";

impl UiScale {
    /// Parse a config value or CLI flag. `None` for anything unrecognised, so
    /// the caller can warn and fall back instead of silently rescaling.
    pub fn parse(value: &str) -> Option<Self> {
        let value = value.trim();
        match value.to_ascii_lowercase().as_str() {
            "" | "default" => Some(DEFAULT_MODE),
            "auto" => Some(Self::Auto),
            "desktop" => Some(Self::Desktop),
            "tv" => Some(Self::Tv),
            _ => value
                .parse::<f32>()
                .ok()
                .filter(|f| f.is_finite() && *f > 0.0)
                .map(|f| Self::Fixed(f.clamp(MIN_SCALE, MAX_SCALE))),
        }
    }
}

/// What the policy knows about the surface the GUI is on.
#[derive(Debug, Clone, Copy)]
pub struct Surface {
    /// Window size in physical pixels.
    pub width: u32,
    pub height: u32,
    /// Factor the windowing system reports for this output.
    pub system_scale: f32,
    /// Panel size in millimetres, when the display told us.
    pub size_mm: Option<(u32, u32)>,
    /// Whether the window covers its whole output.
    pub fullscreen: bool,
}

impl Default for Surface {
    fn default() -> Self {
        Self {
            width: 0,
            height: 0,
            system_scale: 1.0,
            size_mm: None,
            fullscreen: false,
        }
    }
}

/// The scale factor `surface` should be rendered at under `mode`.
pub fn resolve(mode: UiScale, surface: &Surface) -> f32 {
    let scale = match mode {
        UiScale::Fixed(factor) => factor,
        UiScale::Desktop => surface.system_scale,
        UiScale::Tv => tv_scale(surface),
        UiScale::Auto => {
            if looks_like_tv(surface) {
                tv_scale(surface)
            } else {
                surface.system_scale
            }
        }
    };
    scale.clamp(MIN_SCALE, MAX_SCALE)
}

/// The 10-foot factor: the canvas fit, floored by what the platform asks for.
///
/// Without the floor a small window on a HiDPI screen would drop from the
/// system's scale to 1.0, i.e. render at half size. The fit only ever grows the
/// UI, so this is safe to use as the default mode everywhere.
pub fn tv_scale(surface: &Surface) -> f32 {
    fit_design_canvas(surface).max(surface.system_scale)
}

/// Largest factor at which the design canvas still fits the window.
///
/// Snapped down, never below 1.0: a window too small for the canvas keeps the
/// design size and overflows, same as it does today.
pub fn fit_design_canvas(surface: &Surface) -> f32 {
    if surface.width == 0 || surface.height == 0 {
        return 1.0;
    }
    let fit = (surface.width as f32 / DESIGN_WIDTH).min(surface.height as f32 / DESIGN_HEIGHT);
    // Down, not nearest: rounding up would break the fit guarantee.
    let snapped = (fit / SNAP).floor() * SNAP;
    snapped.clamp(1.0, MAX_SCALE)
}

/// Whether this surface should get the 10-foot treatment.
pub fn looks_like_tv(surface: &Surface) -> bool {
    if let Some(size_mm) = surface.size_mm.filter(|(w, h)| *w > 0 && *h > 0) {
        return diagonal_inches(size_mm) >= TV_DIAGONAL_INCHES;
    }

    // No panel size (non-Linux, or a display with no usable EDID): a
    // fullscreen window filling a big output that the platform does not
    // DPI-scale is the TV shape. Platforms that do scale properly report it
    // and land in the desktop branch.
    surface.fullscreen
        && surface.height >= TV_MIN_HEIGHT
        && surface.system_scale <= TV_MAX_SYSTEM_SCALE
}

fn diagonal_inches((width_mm, height_mm): (u32, u32)) -> f32 {
    let (w, h) = (width_mm as f32, height_mm as f32);
    (w * w + h * h).sqrt() / 25.4
}

/// Panel size in millimetres for a connected display, read from the kernel's
/// EDID copy.
///
/// `resolution` picks between multiple connected outputs: the one advertising
/// that mode wins, otherwise the first connected output with a parseable EDID.
/// Best-effort by design, the caller falls back to the shape heuristic.
#[cfg(target_os = "linux")]
pub fn probe_panel_size_mm(resolution: Option<(u32, u32)>) -> Option<(u32, u32)> {
    let entries = match std::fs::read_dir("/sys/class/drm") {
        Ok(entries) => entries,
        Err(err) => {
            debug!(?err, "No DRM sysfs, cannot read panel size");
            return None;
        }
    };

    let mut fallback = None;
    for entry in entries.flatten() {
        let path = entry.path();
        if !std::fs::read_to_string(path.join("status"))
            .is_ok_and(|status| status.trim() == "connected")
        {
            continue;
        }
        let Some(size_mm) = std::fs::read(path.join("edid")).ok().and_then(|edid| {
            let parsed = edid_size_mm(&edid);
            if parsed.is_none() && !edid.is_empty() {
                warn!(?path, len = edid.len(), "Unparseable EDID");
            }
            parsed
        }) else {
            continue;
        };

        let modes = std::fs::read_to_string(path.join("modes")).unwrap_or_default();
        if resolution.is_some_and(|res| advertises_mode(&modes, res)) {
            debug!(?path, ?size_mm, "Panel size from EDID (mode match)");
            return Some(size_mm);
        }
        fallback = fallback.or(Some((path, size_mm)));
    }

    let (path, size_mm) = fallback?;
    debug!(?path, ?size_mm, "Panel size from EDID (first connected)");
    Some(size_mm)
}

/// Current mode of the primary connected output, for deciding the scale before
/// a window exists.
#[cfg(target_os = "linux")]
pub fn probe_output_resolution() -> Option<(u32, u32)> {
    for entry in std::fs::read_dir("/sys/class/drm").ok()?.flatten() {
        let path = entry.path();
        if !std::fs::read_to_string(path.join("status"))
            .is_ok_and(|status| status.trim() == "connected")
        {
            continue;
        }
        // First line is the preferred mode, which is the panel's native
        // resolution on every fixed-pixel display.
        if let Some(mode) = std::fs::read_to_string(path.join("modes"))
            .ok()
            .and_then(|modes| modes.lines().next().and_then(parse_mode))
        {
            debug!(?path, ?mode, "Output resolution from DRM sysfs");
            return Some(mode);
        }
    }
    None
}

#[cfg(not(target_os = "linux"))]
pub fn probe_panel_size_mm(_resolution: Option<(u32, u32)>) -> Option<(u32, u32)> {
    None
}

#[cfg(not(target_os = "linux"))]
pub fn probe_output_resolution() -> Option<(u32, u32)> {
    None
}

#[cfg(any(target_os = "linux", test))]
fn advertises_mode(modes: &str, (width, height): (u32, u32)) -> bool {
    modes
        .lines()
        .filter_map(parse_mode)
        .any(|(w, h)| w == width && h == height)
}

#[cfg(any(target_os = "linux", test))]
fn parse_mode(line: &str) -> Option<(u32, u32)> {
    let (width, height) = line.trim().split_once('x')?;
    // Interlaced modes are written "1920x1080i".
    let height = height.trim_end_matches(['i', 'p']);
    Some((width.parse().ok()?, height.parse().ok()?))
}

/// Physical size in millimetres from a raw EDID blob.
///
/// The base block carries the size in whole centimetres, which is all a
/// TV-vs-monitor decision needs; the first detailed timing descriptor has
/// millimetres and covers displays that leave the coarse fields at zero.
#[cfg(any(target_os = "linux", test))]
fn edid_size_mm(edid: &[u8]) -> Option<(u32, u32)> {
    const MAGIC: [u8; 8] = [0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00];
    if edid.len() < 128 || edid[..8] != MAGIC {
        return None;
    }

    let (width_cm, height_cm) = (edid[21] as u32, edid[22] as u32);
    if width_cm > 0 && height_cm > 0 {
        return Some((width_cm * 10, height_cm * 10));
    }

    // Detailed timing descriptor 1: image size at bytes 12..14 of the
    // descriptor, high nibbles packed into byte 14.
    let dtd = &edid[54..72];
    let width_mm = dtd[12] as u32 | (((dtd[14] >> 4) as u32) << 8);
    let height_mm = dtd[13] as u32 | (((dtd[14] & 0x0F) as u32) << 8);
    (width_mm > 0 && height_mm > 0).then_some((width_mm, height_mm))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn surface(width: u32, height: u32) -> Surface {
        Surface {
            width,
            height,
            ..Default::default()
        }
    }

    /// A 55" 4K panel: 1210x680mm.
    fn tv_surface(width: u32, height: u32) -> Surface {
        Surface {
            width,
            height,
            size_mm: Some((1210, 680)),
            ..Default::default()
        }
    }

    #[test]
    fn parse_modes() {
        // An unset value is the TV policy, not the display sniffing.
        assert_eq!(UiScale::parse(""), Some(DEFAULT_MODE));
        assert_eq!(UiScale::parse("Default"), Some(UiScale::Tv));
        // The drawer offers the default under its own name.
        assert_eq!(UiScale::parse(DEFAULT_MODE_NAME), Some(DEFAULT_MODE));
        assert_eq!(UiScale::parse(" auto "), Some(UiScale::Auto));
        assert_eq!(UiScale::parse("TV"), Some(UiScale::Tv));
        assert_eq!(UiScale::parse("desktop"), Some(UiScale::Desktop));
        assert_eq!(UiScale::parse("2.5"), Some(UiScale::Fixed(2.5)));
        // Out of range clamps rather than rejecting.
        assert_eq!(UiScale::parse("99"), Some(UiScale::Fixed(MAX_SCALE)));
        assert_eq!(UiScale::parse("0.1"), Some(UiScale::Fixed(MIN_SCALE)));
        assert_eq!(UiScale::parse("-2"), None);
        assert_eq!(UiScale::parse("nonsense"), None);
        assert_eq!(UiScale::parse("nan"), None);
    }

    #[test]
    fn canvas_fit_is_exact_on_design_multiples() {
        assert_eq!(fit_design_canvas(&surface(1280, 720)), 1.0);
        assert_eq!(fit_design_canvas(&surface(1920, 1080)), 1.5);
        assert_eq!(fit_design_canvas(&surface(2560, 1440)), 2.0);
        assert_eq!(fit_design_canvas(&surface(3840, 2160)), 3.0);
    }

    #[test]
    fn canvas_fit_never_overflows_the_window() {
        // Every fit must leave the canvas inside the window on both axes.
        for (w, h) in [
            (1366, 768),
            (1600, 900),
            (3840, 1600),
            (2560, 1080),
            (3440, 1440),
            (1920, 1200),
            (4096, 2160),
        ] {
            let scale = fit_design_canvas(&surface(w, h));
            assert!(
                DESIGN_WIDTH * scale <= w as f32 && DESIGN_HEIGHT * scale <= h as f32,
                "{w}x{h} at {scale} does not hold the canvas"
            );
            assert_eq!(scale, (scale / SNAP).round() * SNAP, "{scale} unsnapped");
        }
    }

    #[test]
    fn canvas_fit_floors_at_design_size() {
        assert_eq!(fit_design_canvas(&surface(800, 480)), 1.0);
        // A zero-sized window (before first layout) must not divide by zero.
        assert_eq!(fit_design_canvas(&surface(0, 0)), 1.0);
    }

    #[test]
    fn tv_detection_prefers_panel_size() {
        // 55" panel, not fullscreen, platform reports no scaling.
        assert!(looks_like_tv(&tv_surface(3840, 2160)));
        // 24" 1080p monitor: 530x300mm.
        let monitor = Surface {
            size_mm: Some((530, 300)),
            fullscreen: true,
            ..surface(1920, 1080)
        };
        assert!(!looks_like_tv(&monitor));
    }

    #[test]
    fn tv_detection_falls_back_to_window_shape() {
        let mut s = surface(3840, 2160);
        assert!(!looks_like_tv(&s), "windowed is never a TV");
        s.fullscreen = true;
        assert!(looks_like_tv(&s));
        // A platform that DPI-scales has already sized for the panel.
        s.system_scale = 2.0;
        assert!(!looks_like_tv(&s));
    }

    #[test]
    fn tv_mode_never_undercuts_the_system_scale() {
        // A small window on a HiDPI screen: the canvas does not fit, so the fit
        // is 1.0, but dropping there would render at half size.
        let hidpi_window = Surface {
            system_scale: 2.0,
            ..surface(1000, 600)
        };
        assert_eq!(resolve(UiScale::Tv, &hidpi_window), 2.0);
        // Fullscreen on the same screen: the fit wins because it is bigger.
        let hidpi_full = Surface {
            system_scale: 2.0,
            fullscreen: true,
            ..surface(2880, 1800)
        };
        assert_eq!(resolve(UiScale::Tv, &hidpi_full), 2.25);
    }

    #[test]
    fn resolve_dispatches_per_mode() {
        let tv = tv_surface(3840, 2160);
        assert_eq!(resolve(UiScale::Auto, &tv), 3.0);
        assert_eq!(resolve(UiScale::Tv, &tv), 3.0);
        // Desktop mode is the current behaviour: whatever the platform says.
        let desktop = Surface {
            system_scale: 2.0,
            ..surface(3840, 2160)
        };
        assert_eq!(resolve(UiScale::Desktop, &desktop), 2.0);
        assert_eq!(resolve(UiScale::Auto, &desktop), 2.0);
        assert_eq!(resolve(UiScale::Fixed(1.75), &tv), 1.75);
    }

    /// A minimal but structurally valid EDID.
    fn synthetic_edid(width_cm: u8, height_cm: u8, dtd_mm: Option<(u16, u16)>) -> Vec<u8> {
        let mut edid = vec![0u8; 128];
        edid[..8].copy_from_slice(&[0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00]);
        edid[21] = width_cm;
        edid[22] = height_cm;
        if let Some((w, h)) = dtd_mm {
            edid[54 + 12] = (w & 0xFF) as u8;
            edid[54 + 13] = (h & 0xFF) as u8;
            edid[54 + 14] = (((w >> 8) as u8) << 4) | ((h >> 8) as u8 & 0x0F);
        }
        edid
    }

    #[test]
    fn edid_size_from_base_block() {
        let edid = synthetic_edid(121, 68, None);
        assert_eq!(edid_size_mm(&edid), Some((1210, 680)));
        assert!(diagonal_inches((1210, 680)) > TV_DIAGONAL_INCHES);
    }

    #[test]
    fn edid_size_from_detailed_timing() {
        // Coarse fields zeroed, so the DTD has to carry it.
        let edid = synthetic_edid(0, 0, Some((1209, 681)));
        assert_eq!(edid_size_mm(&edid), Some((1209, 681)));
    }

    #[test]
    fn edid_rejects_garbage() {
        assert_eq!(edid_size_mm(&[]), None);
        assert_eq!(edid_size_mm(&[0u8; 128]), None, "bad magic");
        let short = synthetic_edid(121, 68, None)[..64].to_vec();
        assert_eq!(edid_size_mm(&short), None);
        // Valid header, no size anywhere: a projector-style EDID.
        assert_eq!(edid_size_mm(&synthetic_edid(0, 0, None)), None);
    }

    #[test]
    fn mode_parsing() {
        assert_eq!(parse_mode("3840x2160"), Some((3840, 2160)));
        assert_eq!(parse_mode("1920x1080i"), Some((1920, 1080)));
        assert_eq!(parse_mode(" 1280x720 "), Some((1280, 720)));
        assert_eq!(parse_mode("garbage"), None);
        assert!(advertises_mode("3840x2160\n1920x1080\n", (1920, 1080)));
        assert!(!advertises_mode("3840x2160\n", (1280, 720)));
    }
}
