pub fn hexdump(data: &[u8]) -> String {
    let mut res = String::new();
    macro_rules! maybe_display_char {
        ($f:expr, $byte:expr) => {
            res.push(if $byte.is_ascii() && !$byte.is_ascii_control() {
                $byte as char
            } else {
                '.'
            })
        };
    }
    let chunks = data.chunks_exact(16);
    let rem = chunks.remainder();
    for (i, chunk) in chunks.enumerate() {
        res += &format!("{:08x}: ", i * 16);
        for b in chunk {
            res += &format!("{b:02x} ");
        }
        res += " |";
        for b in chunk {
            maybe_display_char!(f, *b);
        }
        res += "|\n";
    }

    if rem.is_empty() {
        return res;
    }

    res += &format!("{:08x}: ", data.len() / 16 * 16);

    for b in rem {
        res += &format!("{b:02x} ");
    }
    res.push(' ');
    for _ in rem.len()..16 {
        res += "   ";
    }

    res.push('|');
    for b in rem {
        maybe_display_char!(f, *b);
    }
    res += "|\n";
    res
}

// https://github.com/keepsimple1/mdns-sd/blob/52cc67c6a60b6a47553a7f5f1eb0c73a94d20402/src/dns_parser.rs#L1021
pub fn decode_dns_txt(txt: &[u8]) -> Vec<(String, Option<Vec<u8>>)> {
    let mut properties = Vec::new();
    let mut offset = 0;
    while offset < txt.len() {
        let length = txt[offset] as usize;
        if length == 0 {
            break; // reached the end
        }
        offset += 1; // move over the length byte

        let offset_end = offset + length;
        if offset_end > txt.len() {
            break; // Skipping the rest of the record content, as the size for this property would already be out of range.
        }
        let kv_bytes = &txt[offset..offset_end];

        // split key and val using the first `=`
        let (k, v) = kv_bytes.iter().position(|&x| x == b'=').map_or_else(
            || (kv_bytes.to_vec(), None),
            |idx| (kv_bytes[..idx].to_vec(), Some(kv_bytes[idx + 1..].to_vec())),
        );

        // Make sure the key can be stored in UTF-8.
        if let Ok(k_string) = String::from_utf8(k) {
            properties.push((k_string, v));
        }

        offset += length;
    }

    properties
}
