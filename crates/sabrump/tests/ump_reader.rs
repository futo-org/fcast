//! Tests for the UMP wire-format reader.

use bytes::Bytes;
use sabrump::http::SabrBody;
use sabrump::ump::UmpReader;
use sabrump::PartType;

fn write_varint(out: &mut Vec<u8>, value: u64) {
    if value < 128 {
        out.push(value as u8);
    } else if value < 1 << 14 {
        out.push(0x80 | (value & 0x3F) as u8);
        out.push(((value >> 6) & 0xFF) as u8);
    } else if value < 1 << 21 {
        out.push(0xC0 | (value & 0x1F) as u8);
        out.push(((value >> 5) & 0xFF) as u8);
        out.push(((value >> 13) & 0xFF) as u8);
    } else if value < 1 << 28 {
        out.push(0xE0 | (value & 0x0F) as u8);
        out.push(((value >> 4) & 0xFF) as u8);
        out.push(((value >> 12) & 0xFF) as u8);
        out.push(((value >> 20) & 0xFF) as u8);
    } else {
        out.push(0xF0);
        out.push((value & 0xFF) as u8);
        out.push(((value >> 8) & 0xFF) as u8);
        out.push(((value >> 16) & 0xFF) as u8);
        out.push(((value >> 24) & 0xFF) as u8);
    }
}

/// A body that delivers `bytes` one byte per chunk, exercising the reader's
/// incremental parsing across arbitrary chunk boundaries.
fn body(bytes: Vec<u8>) -> SabrBody {
    Box::pin(futures::stream::iter(
        bytes
            .into_iter()
            .map(|b| Ok::<_, std::io::Error>(Bytes::from(vec![b]))),
    ))
}

fn reader(parts: &[(PartType, Vec<u8>)]) -> UmpReader {
    let mut out = Vec::new();
    for (ty, data) in parts {
        write_varint(&mut out, ty.to_wire() as u64);
        write_varint(&mut out, data.len() as u64);
        out.extend_from_slice(data);
    }
    UmpReader::new(body(out))
}

#[tokio::test]
async fn reads_sequence_of_parts() {
    let header: Vec<u8> = (0..3).collect();
    let media: Vec<u8> = (0..500).map(|it| (it % 251) as u8).collect();
    let mut reader = reader(&[
        (PartType::MediaHeader, header.clone()),
        (PartType::Media, media.clone()),
        (PartType::MediaEnd, vec![0]),
    ]);

    let first = reader.next().await.unwrap().unwrap();
    assert_eq!(PartType::MediaHeader, first.ty);
    assert_eq!(header, first.data);

    let second = reader.next().await.unwrap().unwrap();
    assert_eq!(PartType::Media, second.ty);
    assert_eq!(media, second.data);

    let third = reader.next().await.unwrap().unwrap();
    assert_eq!(PartType::MediaEnd, third.ty);

    assert!(reader.next().await.unwrap().is_none());
}

#[tokio::test]
async fn reads_parts_across_every_varint_width() {
    for size in [0usize, 1, 127, 128, 16383, 16384, 100_000] {
        let data: Vec<u8> = (0..size).map(|it| (it % 256) as u8).collect();
        let mut reader = reader(&[(PartType::Media, data.clone())]);
        let part = reader.next().await.unwrap().unwrap();
        assert_eq!(PartType::Media, part.ty, "size={size}");
        assert_eq!(size, part.data.len(), "size={size}");
        assert_eq!(data, part.data, "size={size}");
        assert!(reader.next().await.unwrap().is_none(), "size={size}");
    }
}

#[tokio::test]
async fn reads_large_part_type_and_five_byte_length() {
    let mut out = Vec::new();
    write_varint(&mut out, PartType::SnackbarMessage.to_wire() as u64);
    write_varint(&mut out, 0);
    let mut reader = UmpReader::new(body(out));
    assert_eq!(
        PartType::SnackbarMessage,
        reader.next().await.unwrap().unwrap().ty
    );
}

#[tokio::test]
async fn empty_stream_yields_null() {
    let mut reader = UmpReader::new(body(Vec::new()));
    assert!(reader.next().await.unwrap().is_none());
}
