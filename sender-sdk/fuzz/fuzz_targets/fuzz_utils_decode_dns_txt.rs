#![no_main]

use libfuzzer_sys::fuzz_target;
use utils::decode_dns_txt;

fuzz_target!(|data: &[u8]| {
    let _ = decode_dns_txt(data);
});
