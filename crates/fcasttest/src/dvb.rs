//! Hand-crafted DVB subtitle display sets, for suites that need a bitmap
//! subtitle stream to carry.
//!
//! Like `crate::pgs` and `crate::vobsub`, `fcastplaybin` is a byte pipe for
//! these formats, so a transport test needs a payload with real framing and
//! segments rather than one that decodes to any particular picture. The
//! decoder's own vectors live independently in `fcast-video`'s `subpic::dvb`.
//!
//! Framing: a packet is a PES data field, `[0x20 data identifier][0x00
//! subtitle stream id]`, then segments of `[0x0f sync][type][page id
//! u16be][length u16be][payload]`. A display set is a page composition
//! (0x10), the region compositions it names (0x11), their palettes (0x12)
//! and pixel data (0x13), and an end-of-display-set segment (0x80). Every
//! packet of one display set carries the same timestamp, which is why the
//! driver must forward them all and why a new timestamp ends an unclosed
//! set.

/// `[0x0f][type][page id][length][payload]`.
fn segment(kind: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = vec![0x0F, kind, 0x00, 0x01];
    out.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    out.extend_from_slice(payload);
    out
}

fn data_field(segments: &[Vec<u8>]) -> Vec<u8> {
    let mut out = vec![0x20, 0x00];
    for part in segments {
        out.extend_from_slice(part);
    }
    out
}

/// A 4-bit pixel string of `length` pixels of palette entry 1, then the
/// end-of-string code.
fn four_bit_run(length: u32) -> Vec<u8> {
    // 0000 1 1 10 rrrr iiii, the "run of 9..24" form (covers every length
    // these fixtures use), then 0000 0 000 to end the string.
    let mut bits: Vec<u8> = Vec::new();
    let mut acc: u32 = 0;
    let mut used = 0usize;
    let mut push =
        |value: u32, count: usize, bits: &mut Vec<u8>, acc: &mut u32, used: &mut usize| {
            for step in (0..count).rev() {
                *acc = (*acc << 1) | ((value >> step) & 1);
                *used += 1;
                if *used == 8 {
                    bits.push(*acc as u8);
                    *acc = 0;
                    *used = 0;
                }
            }
        };
    let length = length.clamp(9, 24);
    push(0, 4, &mut bits, &mut acc, &mut used);
    push(1, 1, &mut bits, &mut acc, &mut used);
    push(1, 1, &mut bits, &mut acc, &mut used);
    push(2, 2, &mut bits, &mut acc, &mut used);
    push(length - 9, 4, &mut bits, &mut acc, &mut used);
    push(1, 4, &mut bits, &mut acc, &mut used);
    push(0, 4, &mut bits, &mut acc, &mut used);
    push(0, 1, &mut bits, &mut acc, &mut used);
    push(0, 3, &mut bits, &mut acc, &mut used);
    while used != 0 {
        push(0, 1, &mut bits, &mut acc, &mut used);
    }
    let mut out = vec![0x11];
    out.extend_from_slice(&bits);
    out
}

/// A whole display set in one packet: a 16x4 region at (`100 + tag`, 400) on
/// the standard 720x576 grid, one palette, one object.
///
/// `tag` moves the region so consecutive sets differ in their bytes, which
/// makes a feed log readable.
pub fn display_set(tag: u8) -> Vec<u8> {
    let x = 100u16 + u16::from(tag);
    let mut page = vec![5u8, 0]; // five-second timeout, normal page state
    page.extend_from_slice(&[1, 0]);
    page.extend_from_slice(&x.to_be_bytes());
    page.extend_from_slice(&400u16.to_be_bytes());

    let mut region = vec![1u8, 1 << 3]; // region 1, filled
    region.extend_from_slice(&16u16.to_be_bytes());
    region.extend_from_slice(&4u16.to_be_bytes());
    region.push(2 << 2); // 4-bit depth
    region.push(1); // clut 1
    region.push(0); // 8-bit background
    region.push(0); // 4-bit and 2-bit backgrounds
    region.extend_from_slice(&7u16.to_be_bytes()); // object 7
    region.extend_from_slice(&0u16.to_be_bytes()); // at 0,0
    region.extend_from_slice(&0u16.to_be_bytes());

    // One full-range entry: white, opaque.
    let clut = vec![1u8, 0, 1, 0xE1, 235, 128, 128, 0];

    let field = four_bit_run(16);
    let mut object = vec![0u8, 7, 0]; // object 7, coding method 0
    object.extend_from_slice(&(field.len() as u16).to_be_bytes());
    object.extend_from_slice(&(field.len() as u16).to_be_bytes());
    object.extend_from_slice(&field);
    object.extend_from_slice(&field);

    data_field(&[
        segment(0x10, &page),
        segment(0x11, &region),
        segment(0x12, &clut),
        segment(0x13, &object),
        segment(0x80, &[]),
    ])
}

/// The same display set split across two packets, the way a transport
/// stream delivers one. The segments arrive in several PES packets sharing
/// a timestamp, and only the last carries the end-of-display-set.
pub fn fragmented_display_set(tag: u8) -> Vec<Vec<u8>> {
    let whole = display_set(tag);
    // Split after the page and region segments, a segment boundary by
    // construction. The header is two bytes and each segment carries its own
    // length.
    let mut at = 2usize;
    for _ in 0..2 {
        let length = usize::from(u16::from_be_bytes([whole[at + 4], whole[at + 5]]));
        at += 6 + length;
    }
    let mut first = vec![0x20, 0x00];
    first.extend_from_slice(&whole[2..at]);
    let mut second = vec![0x20, 0x00];
    second.extend_from_slice(&whole[at..]);
    vec![first, second]
}

/// The display set that takes the page away, a page with no regions on it.
pub fn clear_set() -> Vec<u8> {
    data_field(&[segment(0x10, &[5, 0]), segment(0x80, &[])])
}
