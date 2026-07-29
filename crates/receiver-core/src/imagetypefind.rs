//! Custom GStreamer typefinders for image formats the in-tree
//! `typefindfunctions` plugin does not cover (or does not classify the way we
//! need). These let `decodebin3` autoplug our `fimagedec` element (defined
//! elsewhere) by advertising the right media type on the file's magic bytes.
//!
//! Formats handled here:
//!   - JPEG XL (bare codestream and ISOBMFF container): `image/jxl`
//!   - HEIF still-image family (HEIC, AVIF, plain HEIF): `image/heic`,
//!     `image/avif`, `image/heif`
//!   - QOI: `image/qoi`
//!   - farbfeld: `image/x-farbfeld`
//!   - DDS: `image/x-dds`
//!
//! The HEIF finder is the tricky one. The in-tree qt/ISOBMFF typefinder claims
//! these files as "video/quicktime, variant=heif" at MAXIMUM probability
//! because it matches the "mif1" compatible brand. The typefind helper runs
//! finders in rank order and stops at the first MAXIMUM suggestion, so we
//! register our HEIF finder at a rank ABOVE the qt one (PRIMARY + a boost, the
//! in-tree jpeg finder uses PRIMARY + 15 as precedent for a similar boost).

use gst::glib;

/// Every media type string this module's typefinders can suggest. fimagedec
/// must be able to decode all of them (asserted by a test over there, the
/// only consumer, hence the cfg_attr).
#[cfg_attr(not(test), allow(dead_code))]
pub fn produced_caps() -> &'static [&'static str] {
    &[
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
/// still-image ISOBMFF files at MAXIMUM probability via the "mif1" compatible
/// brand. The typefind helper runs finders in descending rank order and stops
/// at the first MAXIMUM suggestion, so a higher rank wins outright. The in-tree
/// jpeg finder uses PRIMARY + 15 as precedent for a rank boost. We go higher so
/// we clear both the base PRIMARY finders and any of their boosts.
const HEIF_RANK_BOOST: i32 = 24;

/// Register every image typefinder this module provides. Call once after
/// `gst::init()`. Registering into the default registry (plugin = None) means
/// `decodebin3` and the type-find helpers pick these up automatically.
pub fn plugin_init() -> Result<(), glib::BoolError> {
    register_jxl()?;
    register_heif()?;
    register_qoi()?;
    register_farbfeld()?;
    register_dds()?;
    Ok(())
}

/// JPEG XL. Two on-disk shapes.
///   - Bare codestream: starts with the 2-byte signature FF 0A.
///   - ISOBMFF container: starts with the 12-byte JXL box signature
///     "\0\0\0\x0cJXL \r\n\x87\n" (a 12-byte box of type "JXL " whose payload
///     is the ISOBMFF signature 0D 0A 87 0A).
fn register_jxl() -> Result<(), glib::BoolError> {
    // 00 00 00 0C  4A 58 4C 20  0D 0A 87 0A
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

/// The ISOBMFF still-image finder (HEIC, AVIF, plain HEIF). See the module doc
/// for why this must outrank the in-tree qt finder.
fn register_heif() -> Result<(), glib::BoolError> {
    // Possible caps covers every media type this one finder can emit, so the
    // helper knows the finder is relevant to those targets.
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

/// HEIC and its HEVC-image-sequence variants. We treat the HEVC sequence brands
/// the same as HEIC.
const HEIC_BRANDS: [&[u8; 4]; 8] = [
    b"heic", b"heix", b"heim", b"heis", b"hevc", b"hevx", b"hevm", b"hevs",
];
/// AVIF still image and image sequence.
const AVIF_BRANDS: [&[u8; 4]; 2] = [b"avif", b"avis"];
/// Plain HEIF: only the generic ISOBMFF still-image brands, none of the codec
/// specific ones above.
const HEIF_BRANDS: [&[u8; 4]; 2] = [b"mif1", b"msf1"];

/// Classify a single codec brand, or `None` if it is not a codec brand. The
/// generic HEIF still-image brands (mif1/msf1) are deliberately excluded here:
/// they only classify a file as plain HEIF when NO codec brand is present
/// anywhere, so they are handled separately.
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

/// Parse an ISOBMFF `ftyp` box and decide which image media type (if any) it
/// declares. Mirrors the brand scan in the in-tree qt finder (peek the box,
/// read the major brand at 8..12, then the compatible brands from 16 onward in
/// 4-byte steps) but only accepts the still-image brands. Plain video mp4
/// brands (isom, mp42, and the like) return `None` so we never steal those.
///
/// Classification order matters. A codec brand (HEIC or AVIF family) anywhere
/// in the box wins over the generic HEIF brand, because a HEIC file commonly
/// carries a mif1 major brand with heic as a compatible brand. The major brand
/// is checked first so it wins among codec brands, then compatible brands in
/// order. Only when no codec brand is present does a mif1/msf1 brand classify
/// the file as plain HEIF.
fn heif_media_type(typefind: &mut gst::TypeFind) -> Option<&'static str> {
    // Need at least the box header plus major brand plus minor version.
    let header = typefind.peek(0, 16)?;

    // bytes 4..8 must be "ftyp", else this is not an ftyp box at all.
    if &header[4..8] != b"ftyp" {
        return None;
    }

    // Box size is a big-endian u32 at 0..4. Sanity-clamp: an ftyp box with a
    // pile of compatible brands is still small, so a size outside [16, 256] is
    // either malformed or a 64-bit size extension we do not need to chase for
    // typefinding. Clamp to the peekable window we care about.
    let raw_size = u32::from_be_bytes([header[0], header[1], header[2], header[3]]) as usize;
    if raw_size < 16 {
        return None;
    }
    let box_size = raw_size.min(256);

    // Peek as much of the box as it claims but tolerate a short peek by only
    // scanning what actually came back. A short source may not hold the whole
    // declared box, in which case we fall back to the 16-byte header we already
    // know is present.
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

    // Then the compatible brands from offset 16 in 4-byte chunks. A codec brand
    // here still wins over a generic HEIF brand, so keep scanning even after we
    // have seen mif1/msf1.
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

    // No codec brand anywhere: plain HEIF if a generic still-image brand was
    // present, otherwise not one of ours (for example a plain video mp4 ftyp).
    if heif_seen { Some("image/heif") } else { None }
}

/// QOI. Magic: the 4 ASCII bytes "qoif" at the start of the file.
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

/// farbfeld. Magic: the 8 ASCII bytes "farbfeld" at the start of the file.
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

/// DDS (DirectDraw Surface). Magic: the 4 bytes "DDS " (44 44 53 20) at the
/// start of the file.
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

    /// Register our finders once, over a `gst::init()`ed registry. The tests
    /// run the real rank-ordered typefind helper (`type_find_helper_for_data`)
    /// so we also exercise the outranking behaviour against the in-tree qt
    /// finder when the built-in typefindfunctions plugin is present.
    fn init() {
        static INIT: Once = Once::new();
        INIT.call_once(|| {
            gst::init().unwrap();
            super::plugin_init().unwrap();
        });
    }

    /// Run the whole rank-ordered typefind helper over `data` and return the
    /// media type name of the winning caps (or None if nothing matched).
    fn detect(data: &[u8]) -> Option<String> {
        let (caps, _prob) = gst_base::type_find_helper_for_data(None::<&gst::Object>, data).ok()?;
        let s = caps.structure(0)?;
        Some(s.name().to_string())
    }

    /// Build a minimal ISOBMFF `ftyp` box: [u32 size][ftyp][major][minor][compat..].
    /// `size` in the header covers the whole box.
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
        // Pad with a little trailing content so short-file helper heuristics
        // have something to look at (real files always have more boxes).
        out.extend_from_slice(&[0u8; 32]);
        out
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
        // Only the generic still-image brand, no codec brand: plain HEIF.
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

    /// A plain video mp4 ftyp (major brand isom, no image brand) must NOT be
    /// claimed by our HEIF finder. It may still be classified as
    /// video/quicktime by the in-tree finder, but it must never come back as
    /// one of our image types.
    #[test]
    fn plain_mp4_not_claimed_as_heif() {
        init();
        let data = ftyp(b"isom", &[b"isom", b"iso2", b"mp41"]);
        let got = detect(&data);
        assert_ne!(got.as_deref(), Some("image/heic"));
        assert_ne!(got.as_deref(), Some("image/avif"));
        assert_ne!(got.as_deref(), Some("image/heif"));
    }

    /// The mif1 compatible brand on an mp42 major must still classify as plain
    /// HEIF for us (the brand really is present), proving the scan reaches the
    /// compatible brands. This is the exact case the in-tree finder would grab
    /// as video/quicktime, so it also proves the rank boost wins.
    #[test]
    fn mif1_compat_beats_intree_quicktime() {
        init();
        let data = ftyp(b"mp42", &[b"mp42", b"mif1"]);
        assert_eq!(detect(&data).as_deref(), Some("image/heif"));
    }

    /// Random bytes with no known magic must match nothing of ours. The helper
    /// may return an error (nothing found) or some unrelated type, but never
    /// one of our image caps.
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
        // Every advertised name must build into a valid Caps structure.
        for name in super::produced_caps() {
            let caps = gst::Caps::builder(*name).build();
            assert_eq!(caps.structure(0).unwrap().name(), *name);
        }
    }
}
