use fcast_sender_sdk::device::{DeviceInfo, ProtocolType};
use std::net::IpAddr;

const DEFAULT_FCAST_PORT: u16 = 46899;
const DEFAULT_GCAST_PORT: u16 = 46899;

fn parse_protocol_type(proto: &str) -> Option<ProtocolType> {
    match proto {
        "fcast" => Some(ProtocolType::FCast),
        "gcast" | "chromecast" => Some(ProtocolType::Chromecast),
        _ => None,
    }
}

fn try_parse_addr(addr: &str) -> Option<IpAddr> {
    let addr = if let Some(stripped) = addr.strip_prefix("//") {
        stripped
    } else {
        addr
    };

    addr.parse::<IpAddr>().ok()
}

// TODO: accept ipv6 addresses
pub fn parse(uri: &str) -> Option<fcast_sender_sdk::device::DeviceInfo> {
    let mut protocol: Option<ProtocolType> = None;
    let mut addr: Option<IpAddr> = None;
    let mut port: Option<u16> = None;

    for segment in uri.split(':') {
        if let Some(parsed_addr) = try_parse_addr(segment) {
            if addr.is_some() {
                return None;
            }
            addr = Some(parsed_addr);
        } else if let Some(parsed_proto) = parse_protocol_type(segment) {
            if protocol.is_some() {
                return None;
            }
            protocol = Some(parsed_proto);
        } else if let Ok(parsed_port) = segment.parse::<u16>() {
            if port.is_some() || parsed_port == 0 {
                return None;
            }
            port = Some(parsed_port);
        } else {
            return None;
        }
    }

    let Some(addr) = addr else {
        return None;
    };
    let protocol = protocol.unwrap_or(ProtocolType::FCast);
    let port = port.unwrap_or(match protocol {
        ProtocolType::Chromecast => DEFAULT_GCAST_PORT,
        ProtocolType::FCast => DEFAULT_FCAST_PORT,
    });
    let name = match protocol {
        ProtocolType::Chromecast => "Chromecast".to_owned(),
        ProtocolType::FCast => "FCast".to_owned(),
    };

    Some(DeviceInfo {
        name,
        protocol,
        addresses: vec![(&addr).into()],
        port,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    macro_rules! f {
        ($addr:expr, $port:expr) => {
            DeviceInfo::fcast("FCast".to_owned(), vec![$addr], $port)
        };
    }

    macro_rules! g {
        ($addr:expr, $port:expr) => {
            DeviceInfo::chromecast("Chromecast".to_owned(), vec![$addr], $port)
        };
    }

    #[test]
    fn test_parse_valid() {
        let localhost: fcast_sender_sdk::IpAddr =
            (&IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)).into();

        let cases = vec![
            ("127.0.0.1", f!(localhost, DEFAULT_FCAST_PORT)),
            ("fcast://127.0.0.1", f!(localhost, DEFAULT_FCAST_PORT)),
            ("127.0.0.1:46899", f!(localhost, DEFAULT_FCAST_PORT)),
            ("127.0.0.1:100", f!(localhost, 100)),
            ("fcast://127.0.0.1:100", f!(localhost, 100)),
            ("gcast://127.0.0.1", g!(localhost, DEFAULT_GCAST_PORT)),
            ("chromecast://127.0.0.1", g!(localhost, DEFAULT_GCAST_PORT)),
            ("gcast://127.0.0.1:100", g!(localhost, 100)),
        ];

        for case in cases {
            assert_eq!(parse(case.0).unwrap(), case.1);
        }
    }

    #[test]
    fn test_parse_invalid() {
        let cases = vec![
            "1270.0.0.1",
            "airplay://127.0.0.1",
            "127.0.0.1:468990",
            "127.0.0.1:0",
        ];

        for case in cases {
            assert_eq!(parse(case), None);
        }
    }
}
