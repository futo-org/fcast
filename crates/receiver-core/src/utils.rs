use std::collections::HashMap;

use tracing::warn;

pub fn sec_to_string(sec: f64) -> String {
    let time_secs = sec % 60.0;
    let time_mins = (sec / 60.0) % 60.0;
    let time_hours = sec / 60.0 / 60.0;

    format!(
        "{:02}:{:02}:{:02}",
        time_hours as u32, time_mins as u32, time_secs as u32,
    )
}

pub fn current_time_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("UNIX_EPOCH is always earlier than now")
        .as_millis() as u64
}

pub fn map_to_header_map(headers: &HashMap<String, String>) -> reqwest::header::HeaderMap {
    let mut header_map = reqwest::header::HeaderMap::new();
    for (k, v) in headers {
        let Ok(name) = reqwest::header::HeaderName::from_bytes(k.as_bytes()) else {
            warn!(k, "Invalid header name");
            continue;
        };
        let Ok(value) = reqwest::header::HeaderValue::from_bytes(v.as_bytes()) else {
            warn!(v, "Invalid header value");
            continue;
        };
        header_map.insert(name, value);
    }

    header_map
}
