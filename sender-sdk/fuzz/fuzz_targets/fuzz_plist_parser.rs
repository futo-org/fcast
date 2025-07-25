#![no_main]

use libfuzzer_sys::fuzz_target;
use plist::{PlistParser, Trailer};

#[derive(arbitrary::Arbitrary, Debug)]
struct FuzzTargetType<'a>(&'a [u8], Trailer);

fuzz_target!(|dat: FuzzTargetType<'_>| {
    if let Ok(mut parser) = PlistParser::new(dat.0, dat.1) {
        let _ = parser.parse();
    }
});
