//! Hand-crafted DVD subpicture units, and the real `.idx` from the sample.
//!
//! The VOBSUB half of what `crate::pgs` does for PGS, and for the same reason:
//! `fcastplaybin` is a byte pipe for bitmap subtitles, so what these suites
//! need is a payload that is REAL (right framing, right commands, a plausible
//! picture), not one that decodes into any particular image. The decoder's own
//! vectors are a separate set of bytes in `fcast-video`'s `subpic::vobsub`,
//! written against the format independently.
//!
//! # The format, in one paragraph
//!
//! A subpicture unit is one self-contained packet: `[size u16be][control offset
//! u16be]`, then run-length-encoded pixel data in two interlaced fields, then a
//! chain of control blocks. Each block is `[delay u16be][next block offset
//! u16be]` followed by commands: set the four palette indices (0x03) and their
//! alphas (0x04), set the display rectangle (0x05), point at the two fields
//! (0x06), display on (0x01), display off (0x02). The chain ends when a
//! block points at itself. Delays are in units of 1024/90000 s from the
//! packet's own timestamp, which is what makes the format self-contained. One
//! packet carries its whole schedule.

/// The `.idx` CodecPrivate of `fcast-sample-media/video/video_with_vobsub.mkv`,
/// read out of the container: 167 bytes, an authoring grid and sixteen RGB
/// palette entries.
///
/// The suites use it as the `codec_data` on synthesized caps so that the
/// transport carries what the container really carries. VOBSUB is the one
/// format whose palette does not travel in the stream.
pub const SAMPLE_IDX: &[u8] = b"size: 720x480\npalette: 0d00ee, ee450d, 101010, ebebeb, 0ce60b, ec14ed, ebff0b, 0d637e, a1a1a1, c5c5c5, 0e640c, 89db89, 0e0089, a2bdd4, ebcf0b, 7e127e\nforced subs: OFF\n";

/// Two nibbles per byte, tail padded.
fn nibbles(values: &[u8]) -> Vec<u8> {
    values
        .chunks(2)
        .map(|pair| (pair[0] << 4) | pair.get(1).copied().unwrap_or(0))
        .collect()
}

/// One RLE run. The low two bits are the palette entry, the rest is the length.
fn run(entry: u8, length: u16) -> Vec<u8> {
    let code = (u32::from(length) << 2) | u32::from(entry & 3);
    match length {
        0..=3 => vec![code as u8],
        4..=15 => vec![(code >> 4) as u8, (code & 0x0F) as u8],
        _ => vec![
            (code >> 8) as u8,
            ((code >> 4) & 0x0F) as u8,
            (code & 0x0F) as u8,
        ],
    }
}

/// One field, a few lines of one colour, byte-aligned per line.
fn field(entry: u8, lines: usize, width: u16) -> Vec<u8> {
    let mut values = Vec::new();
    for _ in 0..lines {
        values.extend_from_slice(&run(entry, width));
        if values.len() % 2 == 1 {
            values.push(0);
        }
    }
    nibbles(&values)
}

/// A whole subpicture unit, an 8x4 picture at (`100 + tag`, 400) on the DVD
/// grid, shown at the packet's own time and taken away half a second later.
///
/// `tag` moves the rectangle so consecutive units differ in their bytes, which
/// makes a feed log readable.
pub fn subpicture_unit(tag: u8) -> Vec<u8> {
    let top = field(1, 2, 8);
    let bottom = field(2, 2, 8);
    let control_at = 4 + top.len() + bottom.len();
    let left = 100 + u32::from(tag);
    let right = left + 7;

    let mut out = vec![0u8, 0, 0, 0];
    out.extend_from_slice(&top);
    out.extend_from_slice(&bottom);

    // Block one: set everything up and display.
    let mut show = vec![
        0x03,
        0x32,
        0x10, // palette indices 0,1,2,3
        0x04,
        0xFF,
        0xF0, // alphas 0,15,15,15
        0x05, // display area, 12-bit coordinates over six bytes
        (left >> 4) as u8,
        ((((left & 0x0F) << 4) | (right >> 8)) as u8),
        (right & 0xFF) as u8,
        (400u32 >> 4) as u8,
        ((((400u32 & 0x0F) << 4) | (403u32 >> 8)) as u8),
        (403u32 & 0xFF) as u8,
        0x06, // the two fields
    ];
    show.extend_from_slice(&(4u16).to_be_bytes());
    show.extend_from_slice(&((4 + top.len()) as u16).to_be_bytes());
    show.push(0x01); // display on
    show.push(0xFF); // end of block

    let mut hide = vec![0x02, 0xFF]; // display off, end of block

    let first_at = control_at;
    let second_at = control_at + 4 + show.len();

    let mut block = Vec::new();
    block.extend_from_slice(&0u16.to_be_bytes()); // no delay
    block.extend_from_slice(&(second_at as u16).to_be_bytes());
    block.append(&mut show);
    out.extend_from_slice(&block);

    let mut block = Vec::new();
    block.extend_from_slice(&45u16.to_be_bytes()); // 45 * 1024/90000 s = 512 ms
    block.extend_from_slice(&(second_at as u16).to_be_bytes()); // points at itself, ending the chain
    block.append(&mut hide);
    out.extend_from_slice(&block);

    let size = out.len() as u16;
    out[0..2].copy_from_slice(&size.to_be_bytes());
    out[2..4].copy_from_slice(&(first_at as u16).to_be_bytes());
    out
}

/// The two-byte delivery a container sends to close an open-ended subtitle.
pub fn terminator() -> Vec<u8> {
    vec![0, 0]
}
