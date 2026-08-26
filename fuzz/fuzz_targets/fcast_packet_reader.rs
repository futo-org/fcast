#![cfg_attr(not(windows), no_main)]

#[cfg(not(windows))]
use libfuzzer_sys::fuzz_target;

#[cfg(not(windows))]
#[derive(Debug, arbitrary::Arbitrary)]
struct Input<'a> {
    max: u16,
    data: &'a [u8],
}

#[cfg(not(windows))]
fuzz_target!(|input: Input<'_>| {
    let mut reader = fcast_protocol::PacketReader::new(input.max as usize, 0);
    if reader.push_data(input.data).is_err() {
        return;
    }
    loop {
        match reader.get_packet() {
            fcast_protocol::ReadResult::NeedData
            | fcast_protocol::ReadResult::PacketTooLarge(_) => break,
            _ => (),
        }
    }
});

// libFuzzer only provides the executable's entry point when the target is
// built through `cargo fuzz`; a plain workspace build on Windows/MSVC fails
// with LNK1561 without one. Fuzzing runs on Linux, so a stub keeps the
// workspace sweep green here.
#[cfg(windows)]
fn main() {}
