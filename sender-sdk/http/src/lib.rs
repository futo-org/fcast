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
    pub const CONTENT_LENGTH: &'static [u8] = b"Content-Length";
    pub const CONTENT_RANGE: &'static [u8] = b"Content-Range";
    pub const CONTENT_TYPE: &'static [u8] = b"Content-Type";
    pub const RANGE: &'static [u8] = b"Range";
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
