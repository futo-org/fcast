#![no_main]

use libfuzzer_sys::fuzz_target;
use http::parse_request_start_line;

fuzz_target!(|data: &[u8]| {
    let _ = parse_request_start_line(data);
});
