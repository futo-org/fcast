#![no_main]

use libfuzzer_sys::fuzz_target;

#[derive(Debug, arbitrary::Arbitrary)]
struct Input<'a> {
    max: u16,
    data: &'a [u8],
}

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
