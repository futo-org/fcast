#![no_main]

use libfuzzer_sys::fuzz_target;
use plist::PlistReader;

fuzz_target!(|plist: &[u8]| {
    let mut reader = PlistReader::new(plist);
    let _ = reader.read_magic_number();
    let _ = reader.read_version();
    let _ = reader.read_trailer();
});
