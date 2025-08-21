#![no_main]

use libfuzzer_sys::fuzz_target;
use parsers_common::parse_header_map;

fuzz_target!(|data: &[u8]| {
    let _ = parse_header_map(data);
});
