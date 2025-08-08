use utils::hexdump;

#[derive(Debug)]
pub enum Version {
    Rtsp10,
}

impl Version {
    pub fn as_str_static(&self) -> &'static str {
        match self {
            Version::Rtsp10 => "RTSP/1.0",
            // Version::Rtsp10 => "HTTP/1.1",
        }
    }
}

#[derive(Debug)]
pub enum Method {
    Post,
    Get,
    Setup,
}

impl Method {
    pub fn as_str_static(&self) -> &'static str {
        match self {
            Method::Post => "POST",
            Method::Get => "GET",
            Method::Setup => "SETUP",
        }
    }
}

pub struct Request<'a> {
    pub method: Method,
    pub path: &'a str,
    pub version: Version,
    pub headers: &'a [(&'a str, &'a str)],
    pub body: Option<&'a [u8]>,
}

impl std::fmt::Debug for Request<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "Method: {:?}", self.method)?;
        writeln!(f, "Path: {}", self.path)?;
        writeln!(f, "Version: {:?}", self.version)?;
        writeln!(f, "Headers: {:?}", self.headers)?;
        if let Some(body) = self.body {
            writeln!(f, "--- Body ---")?;
            write!(f, "{}", hexdump(body))
        } else {
            writeln!(f, "Body: None")
        }
    }
}

impl Request<'_> {
    // TOOD: stream_into()
    pub fn encode_into(&self, buf: &mut Vec<u8>) {
        buf.extend_from_slice(self.method.as_str_static().as_bytes());
        buf.push(b' ');
        buf.extend_from_slice(self.path.as_bytes());
        buf.push(b' ');
        buf.extend_from_slice(self.version.as_str_static().as_bytes());
        buf.extend_from_slice(b"\r\n");

        for header in self.headers {
            buf.extend_from_slice(header.0.as_bytes());
            buf.extend_from_slice(b": ");
            buf.extend_from_slice(header.1.as_bytes());
            buf.extend_from_slice(b"\r\n");
        }

        buf.extend_from_slice(b"\r\n");

        if let Some(body) = self.body {
            buf.extend_from_slice(body);
        }
    }
}

#[derive(Debug, PartialEq)]
pub enum StatusCode {
    Ok,
    InternalServerError,
    Unauthorized,
    Forbidden,
    AuthRequired,
    MethodNotValidInThisState,
}

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum ParseStatusLineError {
    #[error("Unknown RTSP status")]
    UnknownStatus,
}

pub fn parse_response_statusline(statusline: &[u8]) -> Result<StatusCode, ParseStatusLineError> {
    println!("{}", String::from_utf8_lossy(statusline));
    // TODO: properly parse this
    match statusline {
        b"RTSP/1.0 200 OK\r\n" => Ok(StatusCode::Ok),
        b"RTSP/1.0 500 Internal Server Error\r\n" => Ok(StatusCode::InternalServerError),
        b"RTSP/1.0 401 Unauthorized\r\n" => Ok(StatusCode::Unauthorized),
        b"RTSP/1.0 403 Forbidden\r\n" => Ok(StatusCode::Forbidden),
        b"RTSP/1.0 470 Connection Authorization Required\r\n" => Ok(StatusCode::AuthRequired),
        b"RTSP/1.0 455 Method Not Valid In This State\r\n" => Ok(StatusCode::MethodNotValidInThisState),

        // b"HTTP/1.1 200 OK\r\n" => Ok(StatusCode::Ok),
        // b"HTTP/1.1 500 Internal Server Error\r\n" => Ok(StatusCode::InternalServerError),
        // b"HTTP/1.1 470 Connection Authorization Required\r\n" => Ok(StatusCode::AuthRequired),
        _ => Err(ParseStatusLineError::UnknownStatus),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_statusline() {
        {
            assert_eq!(
                parse_response_statusline(b"RTSP/1.0 200 OK\r\n"),
                Ok(StatusCode::Ok)
            );
        }
        {
            assert_eq!(
                parse_response_statusline(b"RTSP/1.0 404 Not Found\r\n"),
                Err(ParseStatusLineError::UnknownStatus)
            );
        }
    }

    #[test]
    fn encode_request() {
        {
            let req = Request {
                method: Method::Post,
                path: "/",
                version: Version::Rtsp10,
                headers: &[("Content-Length", "0")],
                body: None,
            };
            let mut req_buf = Vec::new();
            req.encode_into(&mut req_buf);
            assert_eq!(
                req_buf.as_slice(),
                b"POST / RTSP/1.0\r\n\
                Content-Length: 0\r\n\
                \r\n"
            );
        }
        {
            let req = Request {
                method: Method::Post,
                path: "/",
                version: Version::Rtsp10,
                headers: &[("Content-Length", "13")],
                body: Some(b"Hello, World!"),
            };
            let mut req_buf = Vec::new();
            req.encode_into(&mut req_buf);
            assert_eq!(
                req_buf.as_slice(),
                b"POST / RTSP/1.0\r\n\
                Content-Length: 13\r\n\
                \r\n\
                Hello, World!"
            );
        }
        {
            let req = Request {
                method: Method::Get,
                path: "/",
                version: Version::Rtsp10,
                headers: &[("Content-Length", "0")],
                body: None,
            };
            let mut req_buf = Vec::new();
            req.encode_into(&mut req_buf);
            assert_eq!(
                req_buf.as_slice(),
                b"GET / RTSP/1.0\r\n\
                Content-Length: 0\r\n\
                \r\n"
            );
        }
    }
}
