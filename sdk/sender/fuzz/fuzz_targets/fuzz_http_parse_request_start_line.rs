#![no_main]

use http::parse_request_start_line;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = parse_request_start_line(data);
});
