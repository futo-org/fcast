//! Custom GStreamer typefinders for image formats the in-tree
//! `typefindfunctions` plugin does not cover (or does not classify the way we
//! need), so `decodebin3` can autoplug `fimagedec`: JPEG stills (private
//! `image/x-fcast-jpeg`), JPEG XL, the HEIF still-image family (HEIC, AVIF,
//! plain HEIF), QOI, farbfeld and DDS.

use gst::glib;

/// Every media type this module's typefinders can suggest. fimagedec must
/// decode all of them (asserted there, the only consumer, hence the cfg_attr).
#[cfg_attr(not(test), allow(dead_code))]
pub fn produced_caps() -> &'static [&'static str] {
    &[
        "image/x-fcast-jpeg",
        "image/x-fcast-jpeg-sw",
        "image/jxl",
        "image/heic",
        "image/avif",
        "image/heif",
        "image/qoi",
        "image/x-farbfeld",
        "image/x-dds",
    ]
}

/// The HEIF finder must outrank the in-tree qt/ISOBMFF finder, which claims
/// still-image ISOBMFF at MAXIMUM via the "mif1" compatible brand. The helper
/// runs finders in descending rank order and stops at the first MAXIMUM. This
/// clears the base PRIMARY finders and their boosts (the jpeg one uses +15).
const HEIF_RANK_BOOST: i32 = 24;

/// The JPEG finder must outrank the in-tree jpeg finder (PRIMARY + 15) so a
/// bare JPEG is labeled with our private image/x-fcast-jpeg instead of the
/// MJPEG-shared image/jpeg.
const JPEG_RANK_BOOST: i32 = 24;

/// Register every image typefinder; call once after `gst::init()`. The default
/// registry (plugin = None) is what makes `decodebin3` pick these up.
pub fn plugin_init() -> Result<(), glib::BoolError> {
    register_jpeg()?;
    register_jxl()?;
    register_heif()?;
    register_qoi()?;
    register_farbfeld()?;
    register_dds()?;
    Ok(())
}

/// JPEG still. Magic: SOI plus the next marker's FF, i.e. FF D8 FF. Suggests a
/// private caps rather than image/jpeg so this never competes with
/// avdec_mjpeg/vajpegdec for real MJPEG video. Baseline / extended-sequential
/// (SOF0 / SOF1) get image/x-fcast-jpeg, which fvajpegdec prefers. Everything
/// else gets image/x-fcast-jpeg-sw, which only fimagedec claims (VA JPEG is
/// baseline-only).
fn register_jpeg() -> Result<(), glib::BoolError> {
    const JPEG_MAGIC: [u8; 3] = [0xFF, 0xD8, 0xFF];

    let baseline = gst::Caps::builder("image/x-fcast-jpeg").build();
    let software = gst::Caps::builder("image/x-fcast-jpeg-sw").build();
    let possible = gst::Caps::builder_full()
        .structure(gst::Structure::new_empty("image/x-fcast-jpeg"))
        .structure(gst::Structure::new_empty("image/x-fcast-jpeg-sw"))
        .build();
    gst::TypeFind::register(
        None::<&gst::Plugin>,
        "fjpeg",
        gst::Rank::PRIMARY + JPEG_RANK_BOOST,
        Some("jpg,jpeg,jpe"),
        Some(&possible),
        move |typefind| {
            if typefind.peek(0, 3) != Some(JPEG_MAGIC.as_slice()) {
                return;
            }
            // The SOF can sit well past a large EXIF/APP1 segment, so peek far,
            // with smaller fallbacks for a source that cannot peek that far
            // yet. An unreachable SOF falls back to the software caps.
            const CAP: u32 = 128 * 1024;
            let want = typefind
                .length()
                .filter(|&len| len > 0)
                .map(|len| (len.min(u64::from(CAP))) as u32)
                .unwrap_or(CAP);
            let mut baseline_hint = false;
            for n in [want, 65536, 16384, 4096, 512] {
                if let Some(data) = typefind.peek(0, n) {
                    baseline_hint = jpeg_is_baseline(data);
                    break;
                }
            }
            let caps = if baseline_hint { &baseline } else { &software };
            typefind.suggest(gst::TypeFindProbability::Maximum, caps);
        },
    )
}

/// Walk the JPEG marker segments to the frame header. True only for a baseline
/// / extended-sequential frame (SOF0 / SOF1), what the VA JPEG decoder handles.
/// A malformed header or an SOF past `data` is false.
fn jpeg_is_baseline(data: &[u8]) -> bool {
    // Skip the SOI (FF D8).
    let mut i = 2;
    while i + 1 < data.len() {
        if data[i] != 0xFF {
            return false;
        }
        // The marker code is the next non-fill (non-0xFF) byte.
        let mut j = i + 1;
        while j < data.len() && data[j] == 0xFF {
            j += 1;
        }
        if j >= data.len() {
            return false;
        }
        match data[j] {
            // SOF0 baseline, SOF1 extended sequential.
            0xC0 | 0xC1 => return true,
            // SOF2 progressive and the other SOF types (C4/C8/CC are not SOF).
            0xC2 | 0xC3 | 0xC5..=0xC7 | 0xC9..=0xCB | 0xCD..=0xCF => return false,
            // EOI before any SOF: no frame here.
            0xD9 => return false,
            // Standalone markers with no length payload.
            0x01 | 0xD0..=0xD8 => i = j + 1,
            // Length-bearing segment (APPn, DQT, DHT, COM, ...): the 2-byte
            // big-endian length after the code includes itself.
            _ => {
                if j + 2 >= data.len() {
                    return false;
                }
                let len = u16::from_be_bytes([data[j + 1], data[j + 2]]) as usize;
                if len < 2 {
                    return false;
                }
                i = j + 1 + len;
            }
        }
    }
    false
}

/// JPEG XL. A bare codestream starts with FF 0A. The ISOBMFF container starts
/// with the 12-byte JXL box signature "\0\0\0\x0cJXL \r\n\x87\n".
fn register_jxl() -> Result<(), glib::BoolError> {
    const JXL_CONTAINER_MAGIC: [u8; 12] = [
        0x00, 0x00, 0x00, 0x0C, 0x4A, 0x58, 0x4C, 0x20, 0x0D, 0x0A, 0x87, 0x0A,
    ];
    const JXL_CODESTREAM_MAGIC: [u8; 2] = [0xFF, 0x0A];

    let caps = gst::Caps::builder("image/jxl").build();
    let suggest_caps = caps.clone();
    gst::TypeFind::register(
        None::<&gst::Plugin>,
        "fjxl",
        gst::Rank::PRIMARY,
        Some("jxl"),
        Some(&caps),
        move |typefind| {
            if let Some(data) = typefind.peek(0, 12)
                && data == JXL_CONTAINER_MAGIC
            {
                typefind.suggest(gst::TypeFindProbability::Maximum, &suggest_caps);
                return;
            }
            if let Some(data) = typefind.peek(0, 2)
                && data == JXL_CODESTREAM_MAGIC
            {
                typefind.suggest(gst::TypeFindProbability::Maximum, &suggest_caps);
            }
        },
    )
}

/// The ISOBMFF still-image finder (HEIC, AVIF, plain HEIF).
fn register_heif() -> Result<(), glib::BoolError> {
    // Possible caps must cover every media type this finder can emit, so the
    // helper knows it is relevant to those targets.
    let possible = gst::Caps::builder_full()
        .structure(gst::Structure::new_empty("image/heic"))
        .structure(gst::Structure::new_empty("image/avif"))
        .structure(gst::Structure::new_empty("image/heif"))
        .build();

    gst::TypeFind::register(
        None::<&gst::Plugin>,
        "fheif",
        gst::Rank::PRIMARY + HEIF_RANK_BOOST,
        Some("heic,heif,avif,hif"),
        Some(&possible),
        move |typefind| {
            if let Some(media_type) = heif_media_type(typefind) {
                let caps = gst::Caps::builder(media_type).build();
                typefind.suggest(gst::TypeFindProbability::Maximum, &caps);
            }
        },
    )
}

/// HEIC brands, including the HEVC image-sequence variants.
const HEIC_BRANDS: [&[u8; 4]; 8] = [
    b"heic", b"heix", b"heim", b"heis", b"hevc", b"hevx", b"hevm", b"hevs",
];
/// AVIF still image and image sequence.
const AVIF_BRANDS: [&[u8; 4]; 2] = [b"avif", b"avis"];
/// The generic ISOBMFF still-image brands.
const HEIF_BRANDS: [&[u8; 4]; 2] = [b"mif1", b"msf1"];

/// Classify a single codec brand. mif1/msf1 are excluded: they only mean plain
/// HEIF when no codec brand is present anywhere.
fn codec_brand_media_type(brand: &[u8]) -> Option<&'static str> {
    if HEIC_BRANDS.iter().any(|b| brand == b.as_slice()) {
        return Some("image/heic");
    }
    if AVIF_BRANDS.iter().any(|b| brand == b.as_slice()) {
        return Some("image/avif");
    }
    None
}

/// Whether a brand is one of the generic ISOBMFF still-image brands.
fn is_heif_brand(brand: &[u8]) -> bool {
    HEIF_BRANDS.iter().any(|b| brand == b.as_slice())
}

/// Which image media type an ISOBMFF `ftyp` box declares, if any: major brand
/// at 8..12, compatible brands from 16 onward in 4-byte steps. Plain video mp4
/// brands (isom, mp42, ...) return `None` so we never steal those.
///
/// Order matters: a codec brand (HEIC or AVIF family) anywhere in the box wins
/// over the generic mif1/msf1 brand, since a HEIC file commonly carries a mif1
/// major brand with heic only as a compatible brand.
fn heif_media_type(typefind: &mut gst::TypeFind) -> Option<&'static str> {
    // Need at least the box header plus major brand plus minor version.
    let header = typefind.peek(0, 16)?;

    // Bytes 4..8 must be "ftyp".
    if &header[4..8] != b"ftyp" {
        return None;
    }

    // Box size is a big-endian u32 at 0..4. An ftyp box is always small, so a
    // larger size is malformed or a 64-bit extension we need not chase here.
    let raw_size = u32::from_be_bytes([header[0], header[1], header[2], header[3]]) as usize;
    if raw_size < 16 {
        return None;
    }
    let box_size = raw_size.min(256);

    // Tolerate a short peek by falling back to the 16-byte header we know is
    // present, and only scanning what actually came back.
    let want = box_size as u32;
    let data = match typefind.peek(0, want) {
        Some(d) => d,
        None => typefind.peek(0, 16)?,
    };

    // Major brand first (it wins among codec brands).
    if let Some(media_type) = codec_brand_media_type(&data[8..12]) {
        return Some(media_type);
    }
    let mut heif_seen = is_heif_brand(&data[8..12]);

    // Compatible brands from offset 16 in 4-byte chunks. A codec brand here
    // still wins, so keep scanning after a mif1/msf1 hit.
    let mut off = 16;
    while off + 4 <= data.len() && off < box_size {
        let brand = &data[off..off + 4];
        if let Some(media_type) = codec_brand_media_type(brand) {
            return Some(media_type);
        }
        if is_heif_brand(brand) {
            heif_seen = true;
        }
        off += 4;
    }

    // No codec brand. Plain HEIF only if a generic still-image brand appeared.
    if heif_seen { Some("image/heif") } else { None }
}

/// QOI. Magic: "qoif".
fn register_qoi() -> Result<(), glib::BoolError> {
    let caps = gst::Caps::builder("image/qoi").build();
    let suggest_caps = caps.clone();
    gst::TypeFind::register(
        None::<&gst::Plugin>,
        "fqoi",
        gst::Rank::PRIMARY,
        Some("qoi"),
        Some(&caps),
        move |typefind| {
            if let Some(data) = typefind.peek(0, 4)
                && data == b"qoif"
            {
                typefind.suggest(gst::TypeFindProbability::Maximum, &suggest_caps);
            }
        },
    )
}

/// farbfeld. Magic: "farbfeld".
fn register_farbfeld() -> Result<(), glib::BoolError> {
    let caps = gst::Caps::builder("image/x-farbfeld").build();
    let suggest_caps = caps.clone();
    gst::TypeFind::register(
        None::<&gst::Plugin>,
        "ffarbfeld",
        gst::Rank::PRIMARY,
        Some("ff"),
        Some(&caps),
        move |typefind| {
            if let Some(data) = typefind.peek(0, 8)
                && data == b"farbfeld"
            {
                typefind.suggest(gst::TypeFindProbability::Maximum, &suggest_caps);
            }
        },
    )
}

/// DDS (DirectDraw Surface). Magic: "DDS ".
fn register_dds() -> Result<(), glib::BoolError> {
    let caps = gst::Caps::builder("image/x-dds").build();
    let suggest_caps = caps.clone();
    gst::TypeFind::register(
        None::<&gst::Plugin>,
        "fdds",
        gst::Rank::PRIMARY,
        Some("dds"),
        Some(&caps),
        move |typefind| {
            if let Some(data) = typefind.peek(0, 4)
                && data == b"DDS "
            {
                typefind.suggest(gst::TypeFindProbability::Maximum, &suggest_caps);
            }
        },
    )
}

#[cfg(test)]
mod tests {
    use std::sync::Once;

    /// Register our finders once. Tests run the real rank-ordered typefind
    /// helper, so they also exercise outranking the in-tree finders.
    fn init() {
        static INIT: Once = Once::new();
        INIT.call_once(|| {
            gst::init().unwrap();
            super::plugin_init().unwrap();
        });
    }

    /// Run the typefind helper over `data` and return the winning media type.
    fn detect(data: &[u8]) -> Option<String> {
        let (caps, _prob) = gst_base::type_find_helper_for_data(None::<&gst::Object>, data).ok()?;
        let s = caps.structure(0)?;
        Some(s.name().to_string())
    }

    /// Build a minimal ISOBMFF `ftyp` box: [u32
    /// size][ftyp][major][minor][compat..].
    fn ftyp(major: &[u8; 4], compat: &[&[u8; 4]]) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(major); // major brand at 8..12
        body.extend_from_slice(&[0, 0, 0, 0]); // minor version at 12..16
        for c in compat {
            body.extend_from_slice(*c);
        }
        let box_size = (8 + body.len()) as u32; // 4 (size) + 4 (ftyp) + body
        let mut out = Vec::new();
        out.extend_from_slice(&box_size.to_be_bytes());
        out.extend_from_slice(b"ftyp");
        out.extend_from_slice(&body);
        // Pad so short-file helper heuristics have something to look at.
        out.extend_from_slice(&[0u8; 32]);
        out
    }

    /// A minimal JPEG whose frame header uses marker `sof` (0xC0 = baseline,
    /// 0xC2 = progressive), padded past the smallest peek window so the
    /// typefinder's marker walk actually reaches the SOF.
    fn minimal_jpeg(sof: u8) -> Vec<u8> {
        let mut d = vec![0xFF, 0xD8];
        d.extend_from_slice(&[0xFF, 0xE0, 0x00, 0x10]); // APP0, length 16
        d.extend_from_slice(&[0u8; 14]);
        d.extend_from_slice(&[0xFF, sof, 0x00, 0x11]); // frame header, length 17
        d.extend_from_slice(&[0u8; 15]);
        d.extend_from_slice(&[0u8; 600]);
        d
    }

    #[test]
    fn jpeg_baseline_labeled_for_hw() {
        init();
        // Our higher-ranked finder must win over the in-tree image/jpeg one.
        assert_eq!(
            detect(&minimal_jpeg(0xC0)).as_deref(),
            Some("image/x-fcast-jpeg")
        );
    }

    #[test]
    fn jpeg_progressive_labeled_for_sw() {
        init();
        // A progressive (SOF2) JPEG cannot use the VA decoder.
        assert_eq!(
            detect(&minimal_jpeg(0xC2)).as_deref(),
            Some("image/x-fcast-jpeg-sw")
        );
    }

    #[test]
    fn jxl_bare_codestream() {
        init();
        let mut data = vec![0xFF, 0x0A];
        data.extend_from_slice(&[0u8; 64]);
        assert_eq!(detect(&data).as_deref(), Some("image/jxl"));
    }

    #[test]
    fn jxl_container() {
        init();
        let mut data = vec![
            0x00, 0x00, 0x00, 0x0C, 0x4A, 0x58, 0x4C, 0x20, 0x0D, 0x0A, 0x87, 0x0A,
        ];
        data.extend_from_slice(&[0u8; 64]);
        assert_eq!(detect(&data).as_deref(), Some("image/jxl"));
    }

    #[test]
    fn heif_major_heic() {
        init();
        let data = ftyp(b"heic", &[b"mif1", b"heic"]);
        assert_eq!(detect(&data).as_deref(), Some("image/heic"));
    }

    #[test]
    fn heif_compat_heic_only() {
        init();
        // Major brand is the generic mif1, HEIC declared only as compatible.
        let data = ftyp(b"mif1", &[b"mif1", b"heic"]);
        assert_eq!(detect(&data).as_deref(), Some("image/heic"));
    }

    #[test]
    fn heif_hevc_sequence_maps_to_heic() {
        init();
        let data = ftyp(b"hevc", &[b"mif1", b"hevc"]);
        assert_eq!(detect(&data).as_deref(), Some("image/heic"));
    }

    #[test]
    fn heif_avif() {
        init();
        let data = ftyp(b"avif", &[b"avif", b"mif1"]);
        assert_eq!(detect(&data).as_deref(), Some("image/avif"));
    }

    #[test]
    fn heif_avis_sequence() {
        init();
        let data = ftyp(b"avis", &[b"avis", b"msf1"]);
        assert_eq!(detect(&data).as_deref(), Some("image/avif"));
    }

    #[test]
    fn heif_mif1_only() {
        init();
        let data = ftyp(b"mif1", &[b"mif1"]);
        assert_eq!(detect(&data).as_deref(), Some("image/heif"));
    }

    #[test]
    fn heif_msf1_only() {
        init();
        let data = ftyp(b"msf1", &[b"msf1"]);
        assert_eq!(detect(&data).as_deref(), Some("image/heif"));
    }

    #[test]
    fn qoi_magic() {
        init();
        let mut data = b"qoif".to_vec();
        // A plausible QOI header tail (width/height/channels/colorspace).
        data.extend_from_slice(&[0, 0, 0, 4, 0, 0, 0, 4, 4, 0]);
        assert_eq!(detect(&data).as_deref(), Some("image/qoi"));
    }

    #[test]
    fn farbfeld_magic() {
        init();
        let mut data = b"farbfeld".to_vec();
        data.extend_from_slice(&[0u8; 16]);
        assert_eq!(detect(&data).as_deref(), Some("image/x-farbfeld"));
    }

    #[test]
    fn dds_magic() {
        init();
        let mut data = b"DDS ".to_vec();
        data.extend_from_slice(&[0u8; 124]);
        assert_eq!(detect(&data).as_deref(), Some("image/x-dds"));
    }

    /// A plain video mp4 ftyp must never come back as one of our image types.
    #[test]
    fn plain_mp4_not_claimed_as_heif() {
        init();
        let data = ftyp(b"isom", &[b"isom", b"iso2", b"mp41"]);
        let got = detect(&data);
        assert_ne!(got.as_deref(), Some("image/heic"));
        assert_ne!(got.as_deref(), Some("image/avif"));
        assert_ne!(got.as_deref(), Some("image/heif"));
    }

    /// A mif1 compatible brand on an mp42 major must classify as plain HEIF.
    /// This is the exact case the in-tree finder grabs, so it proves the rank
    /// boost.
    #[test]
    fn mif1_compat_beats_intree_quicktime() {
        init();
        let data = ftyp(b"mp42", &[b"mp42", b"mif1"]);
        assert_eq!(detect(&data).as_deref(), Some("image/heif"));
    }

    /// Random bytes must never come back as one of our image caps.
    #[test]
    fn random_blob_matches_nothing_of_ours() {
        init();
        let data: Vec<u8> = (0u8..=255).cycle().take(512).collect();
        let got = detect(&data);
        for name in super::produced_caps() {
            assert_ne!(got.as_deref(), Some(*name), "unexpectedly matched {name}");
        }
    }

    #[test]
    fn produced_caps_all_valid() {
        init();
        for name in super::produced_caps() {
            let caps = gst::Caps::builder(*name).build();
            assert_eq!(caps.structure(0).unwrap().name(), *name);
        }
    }
}
