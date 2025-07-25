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
            res += &format!("{:02x} ", b);
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
        res += &format!("{:02x} ", b);
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
