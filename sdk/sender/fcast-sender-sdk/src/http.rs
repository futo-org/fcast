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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Protocol {
    Http1,
    Http11,
    Http2,
}

impl Protocol {
    pub fn as_static_str(&self) -> &'static str {
        match self {
            Protocol::Http1 => "HTTP/1.0",
            Protocol::Http11 => "HTTP/1.1",
            Protocol::Http2 => "HTTP/2",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Method {
    Get,
    Post,
    Head,
    Put,
    Options,
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub enum StatusCode {
    Ok,
    ParitalContent,
    BadRequest,
    NotFound,
    MethodNotAllowed,
    InternalServerError,
}

impl StatusCode {
    pub fn as_static_str(&self) -> &'static str {
        match self {
            StatusCode::Ok => "200 OK",
            StatusCode::ParitalContent => "206 Partial Content",
            StatusCode::BadRequest => "400 Bad Request",
            StatusCode::NotFound => "404 Not Found",
            StatusCode::MethodNotAllowed => "405 Method Not Allowed",
            StatusCode::InternalServerError => "500 Internal Server Error",
        }
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq, Clone, Copy)]
pub enum ParseStartLineError {
    #[error("invalid method")]
    InvalidMethod,
    #[error("path is missing")]
    MissingPath,
    #[error("missing protocol")]
    MissingProtocol,
    #[error("invalid protocol")]
    InvalidProtocol,
}

pub fn parse_request_start_line(
    line: &[u8],
) -> Result<(Method, &[u8], Protocol), ParseStartLineError> {
    let methods: [(&[u8], Method); 5] = [
        (b"GET", Method::Get),
        (b"POST", Method::Post),
        (b"HEAD", Method::Head),
        (b"PUT", Method::Put),
        (b"OPTIONS", Method::Options),
    ];

    let (method, method_end_idx) = 'out: {
        for method in methods {
            let method_len = method.0.len();
            if method_len <= line.len() && method.0 == &line[0..method_len] {
                break 'out (method.1, method_len);
            }
        }
        return Err(ParseStartLineError::InvalidMethod);
    };

    if method_end_idx >= line.len() || !line[method_end_idx].is_ascii_whitespace() {
        return Err(ParseStartLineError::MissingPath);
    }

    let path_start_idx = method_end_idx + 1;
    let path_end_idx = {
        let mut i = path_start_idx;
        while i < line.len() && line[i].is_ascii() && !line[i].is_ascii_whitespace() {
            i += 1;
        }
        i
    };

    let protocol_start_idx = path_end_idx + 1;
    if protocol_start_idx >= line.len()
        || path_end_idx - path_start_idx == 0
        || !line[path_end_idx].is_ascii_whitespace()
    {
        return Err(ParseStartLineError::MissingProtocol);
    }

    let path = &line[path_start_idx..path_end_idx];

    let protocols: [(&[u8], Protocol); 3] = [
        (b"HTTP/1.0\r\n", Protocol::Http1),
        (b"HTTP/1.1\r\n", Protocol::Http11),
        (b"HTTP/2\r\n", Protocol::Http2),
    ];

    let protocol = 'out: {
        for protocol in protocols {
            if protocol.0 == &line[protocol_start_idx..] {
                break 'out protocol.1;
            }
        }
        return Err(ParseStartLineError::InvalidProtocol);
    };

    Ok((method, path, protocol))
}

pub struct KnownHeaderNames;

impl KnownHeaderNames {
    pub const CONTENT_LENGTH: &str = "Content-Length";
    pub const CONTENT_RANGE: &str = "Content-Range";
    pub const CONTENT_TYPE: &str = "Content-Type";
    pub const RANGE: &str = "Range";
}

#[derive(Debug, Clone)]
pub struct ResponseStartLine {
    pub protocol: Protocol,
    pub status_code: StatusCode,
}

impl ResponseStartLine {
    pub fn serialize(&self) -> Vec<u8> {
        let mut buf = Vec::new();
        let slices = [
            self.protocol.as_static_str().as_bytes(),
            b" ",
            self.status_code.as_static_str().as_bytes(),
            b"\r\n",
        ];
        for slice in slices {
            buf.extend_from_slice(slice);
        }
        buf
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    macro_rules! m {
        ($method:ident) => {
            Method::$method
        };
    }

    macro_rules! p {
        ($protocol:ident) => {
            Protocol::$protocol
        };
    }

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

    #[test]
    fn valid_parse_request_start_line() {
        let cases: &[(&[u8], (Method, &[u8], Protocol))] = &[
            (b"GET / HTTP/1.0\r\n", (m!(Get), b"/", p!(Http1))),
            (
                b"POST /index HTTP/1.1\r\n",
                (m!(Post), b"/index", p!(Http11)),
            ),
        ];
        for case in cases {
            assert_eq!(parse_request_start_line(case.0).unwrap(), case.1,);
        }
    }

    #[test]
    fn invalid_parse_request_start_line() {
        let cases: &[(&[u8], ParseStartLineError)] = &[
            (b"GeT", ParseStartLineError::InvalidMethod),
            (b"POST", ParseStartLineError::MissingPath),
            (b"POST / FTP\r\n", ParseStartLineError::InvalidProtocol),
        ];
        for case in cases {
            assert_eq!(parse_request_start_line(case.0), Err(case.1), "{case:?}");
        }
    }
}
