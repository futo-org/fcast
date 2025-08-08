use std::collections::HashMap;

/// Find the first `\r\n` sequence in `data` and returns the index of the `\r`.
pub fn find_first_cr_lf(data: &[u8]) -> Option<usize> {
    for (i, win) in data.windows(2).enumerate() {
        if win == *b"\r\n" {
            return Some(i);
        }
    }

    None
}

/// Find the first `\r\n\r\n` sequence in `data` and returns the index of the first `\r`.
pub fn find_first_double_cr_lf(data: &[u8]) -> Option<usize> {
    for (i, win) in data.windows(4).enumerate() {
        if win == b"\r\n\r\n" {
            return Some(i);
        }
    }

    None
}

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum ParseHeaderMapError {
    #[error("Missing key name")]
    MissingKeyName,
    #[error("Missing end CR LF")]
    MissingEndCrLf,
    #[error("Missing value")]
    MissingValue,
    #[error("Missing value (CR LF)")]
    MissingValueCrLf,
    #[error("Malformed header map")]
    Malformed,
    #[error("Key is not UTF-8")]
    KeyIsNotUtf8,
    #[error("Value is not UTF-8")]
    ValueIsNotUtf8,
    #[error("Duplicated header")]
    DuplicatedHeader,
}

/// Parse an RTSP/HTTP header map.
///
/// # Arguments
///   - `data` a byte buffer with key value pairs in the format `<key>: <value>\r\n` that must include
///     the trailing `\r\n` line.
pub fn parse_header_map(data: &[u8]) -> Result<HashMap<&'_ str, &'_ str>, ParseHeaderMapError> {
    let mut map = HashMap::new();
    let mut i = 0;

    while i < data.len() {
        if data[i] == b'\r' {
            break;
        }

        let mut semicolon_idx = i;
        while semicolon_idx < data.len() {
            if data[semicolon_idx] == b':' {
                break;
            }
            semicolon_idx += 1;
        }
        if semicolon_idx >= data.len() || i == semicolon_idx || data[semicolon_idx] != b':' {
            return Err(ParseHeaderMapError::MissingKeyName);
        }

        let key = &data[i..semicolon_idx];

        if semicolon_idx + 1 >= data.len() || data[semicolon_idx + 1] != b' ' {
            return Err(ParseHeaderMapError::MissingValue);
        }

        i = semicolon_idx + 2;

        let mut cr_idx = semicolon_idx + 2;
        while cr_idx < data.len() {
            if data[cr_idx] == b'\r' {
                if cr_idx + 1 >= data.len() || data[cr_idx + 1] != b'\n' {
                    return Err(ParseHeaderMapError::MissingValueCrLf);
                }
                break;
            }
            cr_idx += 1;
        }

        if cr_idx >= data.len() || i == cr_idx || data[cr_idx] != b'\r' {
            return Err(ParseHeaderMapError::MissingValue);
        }

        let value = &data[i..cr_idx];

        i = cr_idx + 2;

        let Ok(key) = str::from_utf8(key) else {
            return Err(ParseHeaderMapError::KeyIsNotUtf8);
        };
        let Ok(value) = str::from_utf8(value) else {
            return Err(ParseHeaderMapError::ValueIsNotUtf8);
        };

        if map.insert(key, value).is_some() {
            return Err(ParseHeaderMapError::DuplicatedHeader);
        }
    }

    if i + 1 >= data.len() || data[i + 1] != b'\n' {
        return Err(ParseHeaderMapError::MissingEndCrLf);
    }

    if i + 1 != data.len() - 1 {
        return Err(ParseHeaderMapError::Malformed);
    }

    Ok(map)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_find_first_cr_lf() {
        assert_eq!(find_first_cr_lf(b"01234\r\n"), Some(5));
        assert_eq!(find_first_cr_lf(b"01234"), None);
        assert_eq!(find_first_cr_lf(b"01234\r\nabc\r\n"), Some(5));
        assert_eq!(find_first_cr_lf(b"\r\n"), Some(0));
        assert_eq!(find_first_cr_lf(b"\r"), None);
        assert_eq!(find_first_cr_lf(b"abc\r"), None);
    }

    #[test]
    fn test_find_first_double_cr_lf() {
        assert_eq!(find_first_double_cr_lf(b"01234\r\n"), None);
        assert_eq!(find_first_double_cr_lf(b"01234\r\n\r\n"), Some(5));
        assert_eq!(find_first_double_cr_lf(b"01234"), None);
        assert_eq!(find_first_double_cr_lf(b"01234\r\nabc\r\n"), None);
        assert_eq!(find_first_double_cr_lf(b"01234\r\n\r\nabc\r\n"), Some(5));
        assert_eq!(find_first_double_cr_lf(b"01234\r\nabc\r\n\r\n"), Some(10));
        assert_eq!(find_first_double_cr_lf(b"\r\n\r\n"), Some(0));
        assert_eq!(find_first_double_cr_lf(b"\r"), None);
        assert_eq!(find_first_double_cr_lf(b"abc\r"), None);
    }

    #[test]
    fn test_parse_valid_header_map() {
        assert_eq!(
            parse_header_map(
                b"Content-Length: 0\r\n\
                    \r\n"
            )
            .unwrap(),
            HashMap::from([("Content-Length", "0"),])
        );
        assert_eq!(
            parse_header_map(
                b"Content-Length: 0\r\n\
                    Content-Type: application/octet-stream\r\n\
                    \r\n"
            )
            .unwrap(),
            HashMap::from([
                ("Content-Length", "0"),
                ("Content-Type", "application/octet-stream",),
            ])
        );
        assert_eq!(parse_header_map(b"\r\n").unwrap(), HashMap::new());
    }

    #[test]
    fn test_parse_invalid_header_map() {
        assert_eq!(
            parse_header_map(b"Content-Length: 0\r\n"),
            Err(ParseHeaderMapError::MissingEndCrLf),
        );
        assert_eq!(
            parse_header_map(b"Content-Length: 0\r\n\r\n this makes it malformed"),
            Err(ParseHeaderMapError::Malformed),
        );
        assert_eq!(
            parse_header_map(b": 0\r\n\r\n"),
            Err(ParseHeaderMapError::MissingKeyName),
        );
        assert_eq!(
            parse_header_map(b"Content-Length: \r\n\r\n"),
            Err(ParseHeaderMapError::MissingValue),
        );
    }
}
