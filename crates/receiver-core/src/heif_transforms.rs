//! The `irot` and `imir` of a HEIF file's primary item, for decoders that
//! hand back the coded image (android's BitmapFactory). HEIF puts the
//! display orientation in these item properties, an Exif orientation in
//! the file is informative only.

use image::metadata::Orientation;

struct Boxes<'a> {
    data: &'a [u8],
}

impl<'a> Iterator for Boxes<'a> {
    /// (type, payload)
    type Item = ([u8; 4], &'a [u8]);

    fn next(&mut self) -> Option<Self::Item> {
        let data = self.data;
        let size = u32::from_be_bytes(data.get(..4)?.try_into().ok()?) as u64;
        let kind: [u8; 4] = data.get(4..8)?.try_into().ok()?;
        let (header, size) = match size {
            0 => (8, data.len() as u64),
            1 => (16, u64::from_be_bytes(data.get(8..16)?.try_into().ok()?)),
            size => (8, size),
        };
        let size = usize::try_from(size).ok()?;
        let payload = data.get(header..size)?;
        self.data = &data[size..];
        Some((kind, payload))
    }
}

fn boxes(data: &[u8]) -> Boxes<'_> {
    Boxes { data }
}

fn child<'a>(data: &'a [u8], kind: &[u8; 4]) -> Option<&'a [u8]> {
    boxes(data).find(|(k, _)| k == kind).map(|(_, payload)| payload)
}

/// A full box's version, flags and body.
fn full(payload: &[u8]) -> Option<(u8, u32, &[u8])> {
    let head = u32::from_be_bytes(payload.get(..4)?.try_into().ok()?);
    Some(((head >> 24) as u8, head & 0xff_ffff, &payload[4..]))
}

fn be(data: &[u8], at: usize, len: usize) -> Option<u32> {
    data.get(at..at + len)?
        .iter()
        .try_fold(0u32, |acc, b| Some(acc << 8 | u32::from(*b)))
}

/// None for a file that is not HEIF or is malformed, which then shows as
/// coded.
pub fn primary_orientation(data: &[u8]) -> Option<Orientation> {
    let (_, _, meta) = full(child(data, b"meta")?)?;
    let (version, _, pitm) = full(child(meta, b"pitm")?)?;
    let primary = be(pitm, 0, if version == 0 { 2 } else { 4 })?;

    let iprp = child(meta, b"iprp")?;
    let properties: smallvec::SmallVec<[([u8; 4], &[u8]); 16]> =
        boxes(child(iprp, b"ipco")?).collect();
    let (version, flags, ipma) = full(child(iprp, b"ipma")?)?;
    let id_len = if version == 0 { 2 } else { 4 };
    let index_len = if flags & 1 != 0 { 2 } else { 1 };

    let mut rotation = 0;
    let mut mirror = None;
    let mut at = 4;
    for _ in 0..be(ipma, 0, 4)? {
        let item = be(ipma, at, id_len)?;
        let count = usize::from(*ipma.get(at + id_len)?);
        at += id_len + 1;
        if item == primary {
            for i in 0..count {
                let raw = be(ipma, at + i * index_len, index_len)?;
                // the top bit is the essential flag, 0 means no property
                let index = raw & !(1 << (index_len * 8 - 1));
                let Some((kind, payload)) = (index as usize)
                    .checked_sub(1)
                    .and_then(|i| properties.get(i))
                else {
                    continue;
                };
                match kind {
                    b"irot" => rotation = payload.first()? & 3,
                    b"imir" => mirror = Some(payload.first()? & 1),
                    _ => {}
                }
            }
            return Some(orientation_from(rotation, mirror));
        }
        at += count * index_len;
    }
    None
}

/// HEIF applies `irot` (quarter turns anticlockwise) before `imir` (mode 0
/// exchanges top and bottom, mode 1 left and right).
fn orientation_from(rotation: u8, mirror: Option<u8>) -> Orientation {
    match (rotation, mirror) {
        (0, None) => Orientation::NoTransforms,
        (1, None) => Orientation::Rotate270,
        (2, None) => Orientation::Rotate180,
        (_, None) => Orientation::Rotate90,
        (0, Some(1)) | (2, Some(0)) => Orientation::FlipHorizontal,
        (0, Some(_)) | (2, Some(_)) => Orientation::FlipVertical,
        (1, Some(1)) | (3, Some(0)) => Orientation::Rotate270FlipH,
        (_, Some(_)) => Orientation::Rotate90FlipH,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 64x32 HEICs coded rotated or mirrored, each with the irot/imir that
    /// undoes it (Exif orientations 1 to 8, made with heif-enc, which also
    /// writes a clap and keeps the Exif orientation item).
    const ORIENTED: [&[u8]; 8] = [
        include_bytes!("../test-data/oriented-heic/o1.heic"),
        include_bytes!("../test-data/oriented-heic/o2.heic"),
        include_bytes!("../test-data/oriented-heic/o3.heic"),
        include_bytes!("../test-data/oriented-heic/o4.heic"),
        include_bytes!("../test-data/oriented-heic/o5.heic"),
        include_bytes!("../test-data/oriented-heic/o6.heic"),
        include_bytes!("../test-data/oriented-heic/o7.heic"),
        include_bytes!("../test-data/oriented-heic/o8.heic"),
    ];

    #[test]
    fn every_orientation_is_read() {
        for (i, data) in ORIENTED.iter().enumerate() {
            let exif = i as u8 + 1;
            assert_eq!(
                primary_orientation(data).map(Orientation::to_exif),
                Some(exif),
                "o{exif}"
            );
        }
    }

    #[test]
    fn transforms_map_to_exif_orientations() {
        let cases = [
            (0, None, 1),
            (0, Some(1), 2),
            (2, None, 3),
            (0, Some(0), 4),
            (3, Some(1), 5),
            (3, None, 6),
            (3, Some(0), 7),
            (1, None, 8),
            (2, Some(0), 2),
            (2, Some(1), 4),
            (1, Some(0), 5),
            (1, Some(1), 7),
        ];
        for (rotation, mirror, exif) in cases {
            assert_eq!(
                orientation_from(rotation, mirror).to_exif(),
                exif,
                "irot {rotation} imir {mirror:?}"
            );
        }
    }

    #[test]
    fn malformed_input_is_none() {
        assert_eq!(primary_orientation(b""), None);
        assert_eq!(primary_orientation(b"\0\0\0\x08ftyp"), None);
        let o6 = ORIENTED[5];
        for len in [12, 40, o6.len() / 2] {
            // truncated anywhere, never a panic
            let _ = primary_orientation(&o6[..len]);
        }
        // a box claiming more than the file holds
        let mut lying = o6.to_vec();
        lying[..4].copy_from_slice(&u32::MAX.to_be_bytes());
        let _ = primary_orientation(&lying);
    }
}
