use std::collections::HashMap;

use anyhow::{anyhow, ensure, Context};

const FRAGMENT_THRESHOLD: usize = 0xFF;

#[derive(Debug, thiserror::Error)]
pub enum TagError {
    #[error("Invalid value `{0}`")]
    InvalidValue(u8),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Tag {
    Method = 0x0,
    Identifier = 0x1,
    Salt = 0x2,
    PublicKey = 0x3,
    Proof = 0x4,
    EncryptedData = 0x5,
    State = 0x6,
    Error = 0x7,
    RetryDelay = 0x8,
    Certificate = 0x9,
    Signature = 0x0A,
    Permissions = 0x0B,
    FragmentData = 0x0C,
    FragmentLast = 0x0D,
    Flags = 0x13,
    Separator = 0xFF,
}

impl TryFrom<u8> for Tag {
    type Error = TagError;

    fn try_from(value: u8) -> Result<Self, TagError> {
        match value {
            0x00 => Ok(Self::Method),
            0x01 => Ok(Self::Identifier),
            0x02 => Ok(Self::Salt),
            0x03 => Ok(Self::PublicKey),
            0x04 => Ok(Self::Proof),
            0x05 => Ok(Self::EncryptedData),
            0x06 => Ok(Self::State),
            0x07 => Ok(Self::Error),
            0x08 => Ok(Self::RetryDelay),
            0x09 => Ok(Self::Certificate),
            0x0A => Ok(Self::Signature),
            0x0B => Ok(Self::Permissions),
            0x0C => Ok(Self::FragmentData),
            0x0D => Ok(Self::FragmentLast),
            0x13 => Ok(Self::Flags),
            0xFF => Ok(Self::Separator),
            _ => Err(TagError::InvalidValue(value)),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Item {
    pub tag: Tag,
    pub value: Vec<u8>, // TODO: store enum with either vec or slice (with lifetime) for more efficient memory use
}

impl Item {
    pub fn new(tag: Tag, value: Vec<u8>) -> Self {
        Self { tag, value }
    }
}

fn fragment_standard(items: &[Item]) -> Vec<Item> {
    let mut frags = Vec::new();
    for item in items {
        if item.value.len() <= FRAGMENT_THRESHOLD {
            frags.push(item.clone());
        } else {
            let mut offset = 0;
            frags.push(Item {
                tag: item.tag,
                value: item.value[offset..FRAGMENT_THRESHOLD].to_vec(),
            });
            offset += FRAGMENT_THRESHOLD;

            while item.value.len() - offset > FRAGMENT_THRESHOLD {
                frags.push(Item {
                    tag: Tag::FragmentData,
                    value: item.value[offset..offset + FRAGMENT_THRESHOLD].to_vec(),
                });
                offset += FRAGMENT_THRESHOLD;
            }

            frags.push(Item {
                tag: Tag::FragmentLast,
                value: item.value[offset..item.value.len()].to_vec(),
            });
        }
    }

    frags
}

fn fragment_repeat(items: &[Item]) -> Vec<Item> {
    let mut frags = Vec::new();
    for item in items {
        let bytes = &item.value;
        let mut offset = 0;
        while offset < bytes.len() {
            let chunk = FRAGMENT_THRESHOLD.min(bytes.len() - offset);
            frags.push(Item::new(item.tag, bytes[offset..offset + chunk].to_vec()));
            offset += chunk;
        }
    }

    frags
}

pub fn encode(items: &[Item], use_fragment_data: bool) -> Vec<u8> {
    let total_size = items.iter().map(|item| 2 + item.value.len()).sum();
    let mut data = Vec::with_capacity(total_size);

    let fragments = if use_fragment_data {
        fragment_standard(items)
    } else {
        fragment_repeat(items)
    };
    for frag in fragments {
        data.push(frag.tag as u8);
        data.push(frag.value.len() as u8);
        data.extend_from_slice(&frag.value);
    }

    data
}

pub fn decode(data: &[u8]) -> anyhow::Result<Vec<Item>> {
    let mut items = Vec::new();

    let mut i = 0;
    while i < data.len() {
        let tag = Tag::try_from(data[i]).with_context(|| format!("Unknown tag at offset {i}"))?;

        let length = *data.get(i + 1).ok_or(anyhow!(
            "Truncated TLV: no length byte for tag {tag:?} at offset {i}"
        ))? as usize;

        log::debug!("Decode: TLV item with tag={tag:?} len={length}");

        i += 2;
        ensure!(
            i + length <= data.len(),
            "Truncated TLV: declared length {length} exceeds available bytes ({})",
            data.len() - i
        );
        let mut value = data[i..i + length].to_vec();
        i += length;

        if length == FRAGMENT_THRESHOLD && i < data.len() {
            let next_tag =
                Tag::try_from(data[i]).with_context(|| format!("Unknown tag at offset {i}"))?;
            if next_tag == Tag::FragmentData || next_tag == Tag::FragmentLast || next_tag == tag {
                loop {
                    ensure!(
                        i + 2 < data.len(),
                        "Truncated fragment header at offset {i}"
                    );
                    let frag_tag = Tag::try_from(data[i])
                        .with_context(|| format!("Unknown fragment tag at offset {i}"))?;
                    ensure!(
                        frag_tag == Tag::FragmentData
                            || frag_tag == Tag::FragmentLast
                            || frag_tag == tag,
                        "Unexpected tag `{frag_tag:?}` in fragment sequence"
                    );

                    let frag_len = data[i + 1] as usize;
                    i += 2;
                    ensure!(
                        i + frag_len <= data.len(),
                        "Truncated fragment: declared length {frag_len} exceeds available bytes ({})",
                        data.len() - i
                    );

                    value.extend_from_slice(&data[i..i + frag_len]);
                    i += frag_len;

                    if frag_tag == Tag::FragmentLast || frag_len < FRAGMENT_THRESHOLD {
                        break;
                    }
                }
            }
        }

        items.push(Item { tag, value })
    }

    Ok(items)
}

pub fn mapify(items: Vec<Item>) -> HashMap<Tag, Vec<u8>> {
    let mut item_map = HashMap::new();
    for item in items {
        item_map.insert(item.tag, item.value);
    }

    item_map
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_and_decode_simple_small_value() {
        let value = [0x01, 0x02, 0x03, 0x04].to_vec();
        let item = Item::new(Tag::Method, value.clone());

        let encoded = encode(&[item.clone()], false);
        let decoded = decode(&encoded).unwrap();

        assert_eq!(decoded, vec![item]);
    }

    #[test]
    fn encode_and_decode_exactly_255_bytes_no_fragmentation() {
        let data_255 = [0u8; 255];
        let item_255 = Item::new(Tag::Identifier, data_255.to_vec());

        let encoded = encode(&[item_255.clone()], false);
        assert_eq!(encoded.len(), 257);
        assert_eq!(Tag::Identifier as u8, encoded[0]);
        assert_eq!(0xFF, encoded[1]);

        let decoded = decode(&encoded).unwrap();
        assert_eq!(decoded, vec![item_255]);
    }

    #[test]
    fn encode_and_decode256_bytes_with_fragmentation() {
        let data_256 = [0u8; 256];
        let item_256 = Item::new(Tag::Salt, data_256.to_vec());

        let encoded = encode(&[item_256.clone()], true);
        assert_eq!(Tag::Salt as u8, encoded[0]);
        assert_eq!(0xFF, encoded[1]);

        // Locate last‐fragment header: two bytes before the final data byte
        let last_fragment_index = encoded.len() - (1 /*remaining*/ + 2);
        assert_eq!(Tag::FragmentLast as u8, encoded[last_fragment_index]);
        assert_eq!(1, encoded[last_fragment_index + 1]);

        let decoded = decode(&encoded).unwrap();
        assert_eq!(decoded, vec![item_256]);
    }

    #[test]
    fn test_encode_and_decode_multiple_items() {
        let v1 = [0x0A, 0x0B];
        let v2 = [0xFF, 0xEE, 0xDD];
        let items = [
            Item::new(Tag::Proof, v1.to_vec()),
            Item::new(Tag::Error, v2.to_vec()),
        ];

        let encoded = encode(&items.clone(), false);
        let decoded = decode(&encoded).unwrap();

        assert_eq!(decoded, items);
    }

    #[test]
    fn test_decode_unknown_tag_throws_illegal_argument_exception() {
        // Tag 0x10 isn’t defined in TLV8Tag
        let bogus = [0x10, 0x00];
        assert!(decode(&bogus).is_err());
    }

    #[test]
    fn test_decode_truncated_length_byte_throws_illegal_argument_exception() {
        // Only a tag byte, missing length byte
        let only_tag = [Tag::State as u8];
        assert!(decode(&only_tag).is_err());
    }

    #[test]
    fn test_decode_truncated_data_throws_illegal_argument_exception() {
        // Declared length = 2, but only 1 data byte follows
        let arr = [Tag::Flags as u8, 2, 0x5A];
        assert!(decode(&arr).is_err());
    }
}
