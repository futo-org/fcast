#![cfg_attr(not(windows), no_main)]

#[cfg(not(windows))]
use fcast_protocol::{v2, v3, Opcode, PlaybackErrorMessage, VersionMessage};
#[cfg(not(windows))]
use libfuzzer_sys::fuzz_target;

#[cfg(not(windows))]
#[derive(Debug, arbitrary::Arbitrary)]
struct Input<'a> {
    version: u8,
    data: &'a [u8],
}

// What a v2 or v3 sender reads, decoded with the serde types
#[cfg(not(windows))]
fn read(opcode: u8, body: &[u8]) {
    let Ok(op) = Opcode::try_from(opcode) else {
        return;
    };
    match op {
        Opcode::PlaybackUpdate => {
            let _ = serde_json::from_slice::<v2::PlaybackUpdateMessage>(body);
            let _ = serde_json::from_slice::<v3::PlaybackUpdateMessage>(body);
        }
        Opcode::VolumeUpdate => {
            let _ = serde_json::from_slice::<v2::VolumeUpdateMessage>(body);
        }
        Opcode::PlaybackError => {
            let _ = serde_json::from_slice::<PlaybackErrorMessage>(body);
        }
        Opcode::Version => {
            let _ = serde_json::from_slice::<VersionMessage>(body);
        }
        Opcode::Initial => {
            let _ = serde_json::from_slice::<v3::InitialReceiverMessage>(body);
        }
        _ => {}
    }
}

// Splits frames off the input and reads each as a receiver message
#[cfg(not(windows))]
fuzz_target!(|input: Input<'_>| {
    let max = fcast_protocol::max_packet_size(u64::from(input.version));
    let mut rest = input.data;
    while let Ok(Some((frame, used))) = fcast_protocol::split_frame(rest, max) {
        assert!(used > 0 && used <= rest.len());
        rest = &rest[used..];
        read(frame.opcode, frame.body);
    }
    // A bare body read as every opcode
    for op in 0..=u8::MAX {
        read(op, input.data);
    }
});

// See fcast_packet_reader.rs
#[cfg(windows)]
fn main() {}
