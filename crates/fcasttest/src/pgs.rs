//! Hand-crafted PGS display sets, for suites that need a bitmap subtitle
//! stream to carry.
//!
//! No PGS encoder exists, so a PGS fixture is bytes written out field by
//! field. `fcastplaybin` is a byte pipe for bitmap subtitles (it decides the
//! format from caps, converts the timestamp and forwards the buffer
//! untouched), so what these tests need from a display set is real framing,
//! real segments, a plausible size, not any particular picture.
//!
//! The decoder's own vectors are a different set of bytes on purpose, in
//! `fcast-video`'s `subpic::pgs::fixtures`. A fixture generator shared with
//! the thing it tests can hide a mutual misunderstanding.
//!
//! Framing: a PGS stream is segments of `[type u8][length u16be][payload]`.
//! A display set opens with a presentation segment (0x16, the canvas and
//! where each object goes on it), carries a window segment (0x17), a palette
//! segment (0x14, 5-byte `index, Y, Cr, Cb, A` entries) and one or more
//! object segments (0x15, a run-length-encoded picture, possibly split
//! across segments), and closes with an end segment (0x80).

/// `[type u8][length u16be][payload]`.
fn segment(kind: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = vec![kind];
    out.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    out.extend_from_slice(payload);
    out
}

/// A 4x2 run-length-encoded block of one palette entry, behind the
/// `width, height` header every PGS object carries.
fn picture() -> Vec<u8> {
    let mut out = 4u16.to_be_bytes().to_vec();
    out.extend_from_slice(&2u16.to_be_bytes());
    for _ in 0..2 {
        // A run of four pixels of palette entry 1, then the zero-length run
        // that ends a line.
        out.extend_from_slice(&[0x00, 0x84, 0x01, 0x00, 0x00]);
    }
    out
}

fn presentation(objects: u8, tag: u8) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&1920u16.to_be_bytes()); // canvas width
    payload.extend_from_slice(&1080u16.to_be_bytes()); // canvas height
    payload.push(0x10); // frame rate code
    payload.extend_from_slice(&u16::from(tag).to_be_bytes()); // composition number
    payload.push(0x80); // composition state: epoch start
    payload.push(0x00); // flags
    payload.push(0x00); // palette id
    payload.push(objects);
    for _ in 0..objects {
        payload.extend_from_slice(&1u16.to_be_bytes()); // object id
        payload.push(0); // window id
        payload.push(0); // flags: not cropped, not forced
        // Position on the canvas, moved per set so consecutive display sets are
        // not byte-identical.
        payload.extend_from_slice(&(100 + u16::from(tag)).to_be_bytes());
        payload.extend_from_slice(&900u16.to_be_bytes());
    }
    segment(0x16, &payload)
}

fn window() -> Vec<u8> {
    let mut payload = vec![1, 0]; // one window, id 0
    for value in [100u16, 900, 4, 2] {
        payload.extend_from_slice(&value.to_be_bytes());
    }
    segment(0x17, &payload)
}

fn palette() -> Vec<u8> {
    // Palette id, version, then one opaque white entry at index 1.
    segment(0x14, &[0, 0, 1, 235, 128, 128, 255])
}

fn object(first: bool, total: usize, body: &[u8]) -> Vec<u8> {
    let mut payload = 1u16.to_be_bytes().to_vec(); // object id
    payload.push(0); // version
    payload.push(if first { 0x80 } else { 0x00 });
    if first {
        payload.extend_from_slice(&[(total >> 16) as u8, (total >> 8) as u8, total as u8]);
    }
    payload.extend_from_slice(body);
    segment(0x15, &payload)
}

fn end() -> Vec<u8> {
    segment(0x80, &[])
}

/// A whole display set in one packet. `tag` moves the composition so that
/// consecutive sets differ in their bytes, which makes a feed log readable.
pub fn display_set(tag: u8) -> Vec<u8> {
    let picture = picture();
    [
        presentation(1, tag),
        window(),
        palette(),
        object(true, picture.len(), &picture),
        end(),
    ]
    .concat()
}

/// The same display set split across two packets, mid-object. The shape a
/// display set has when its object does not fit one segment, and the reason
/// the driver must forward every buffer rather than only the ones that look
/// complete.
pub fn fragmented_display_set(tag: u8) -> Vec<Vec<u8>> {
    let picture = picture();
    let (head, tail) = picture.split_at(6);
    vec![
        [
            presentation(1, tag),
            window(),
            palette(),
            object(true, picture.len(), head),
        ]
        .concat(),
        [object(false, 0, tail), end()].concat(),
    ]
}

/// The set that takes a subtitle off the screen, a composition with no
/// objects in it.
pub fn clear_set() -> Vec<u8> {
    [presentation(0, 0), end()].concat()
}
