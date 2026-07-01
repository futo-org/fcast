//! AirPlay mirror audio: AAC-ELD over RTP/UDP.
//!
//! Mirror audio is a separate stream from the video (`type 96` in `SETUP`,
//! versus `type 110` for video). It arrives as RTP/UDP packets - a 12-byte RTP
//! header followed by an AES-128-CBC-encrypted AAC-ELD (enhanced low delay) frame (`raop_rtp.c`,
//! `raop_buffer.c`):
//!
//! ```text
//! [0]      0x80
//! [1]      0x60  (payload type 96)
//! [2..4]   sequence number   (BE16)
//! [4..8]   RTP timestamp     (BE32, 44.1 kHz)
//! [8..12]  zero
//! [12..]   encrypted AAC-ELD frame
//! ```
//!
//! Two quirks (`raop_rtp.c`): the stream opens with "no data" packets whose
//! payload is the 4-byte marker `00 68 34 00`, and every frame is transmitted
//! three times with incrementing sequence numbers - so we de-duplicate by
//! keeping only strictly-newer sequence numbers.
//!
//! This task only receives, decrypts and de-duplicates; the decrypted AAC-ELD
//! frames are handed to the `airplaysrc` Bin's audio appsrc (via the shared
//! [`AirPlayContext`](super::source::AirPlayContext)) so they decode inside the
//! shared `playbin3` pipeline alongside the video - one clock, one volume.

use std::{sync::mpsc::Sender, time::Duration};

use aes::{
    Aes128,
    cipher::{
        BlockDecryptMut, InnerIvInit, KeyInit, block_padding::NoPadding,
        generic_array::GenericArray,
    },
};
use tokio::net::UdpSocket;
use tracing::{debug, info, instrument, warn};

use super::source::AudioFrame;

type Aes128CbcDec = cbc::Decryptor<Aes128>;

/// AAC-ELD compression type (`ct`) in the `SETUP` audio stream.
pub const CT_AAC_ELD: u8 = 8;

/// End the audio session if no packet arrives for this long (the client stops
/// sending on teardown; UDP gives us no explicit close).
const IDLE_TIMEOUT: Duration = Duration::from_secs(10);

/// Decrypt an audio payload in place-ish: AES-128-CBC over the whole 16-byte
/// blocks (IV reset per packet), the trailing partial block left as plaintext
/// (`raop_buffer.c`).
fn decrypt(cipher: &Aes128, aesiv: &[u8; 16], payload: &[u8]) -> Vec<u8> {
    let block_len = payload.len() & !0xf;
    let mut out = payload.to_vec();
    let iv = GenericArray::from_slice(aesiv);
    let decryptor = Aes128CbcDec::inner_iv_init(cipher.clone(), iv);
    // NoPadding over a block-aligned length never fails.
    if let Err(err) = decryptor.decrypt_padded_mut::<NoPadding>(&mut out[..block_len]) {
        warn!(?err, "mirror audio decrypt failed");
    }
    out
}

/// True if `seq` is strictly newer than `last` under 16-bit wraparound.
fn seq_newer(seq: u16, last: u16) -> bool {
    seq != last && seq.wrapping_sub(last) < 0x8000
}

/// Receive, decrypt and de-duplicate the mirror audio stream, handing each
/// decrypted AAC-ELD frame to `frame_tx` (the `airplaysrc` audio appsrc). Ends
/// when the client stops sending (idle timeout), the socket errors, or the
/// source drops the receiver.
#[instrument(skip_all, fields(device = %device_name))]
pub async fn run(
    socket: UdpSocket,
    aeskey: [u8; 16],
    aesiv: [u8; 16],
    ct: u8,
    frame_tx: Sender<AudioFrame>,
    device_name: String,
) {
    if ct != CT_AAC_ELD {
        warn!(
            ct,
            "unsupported mirror audio compression type, audio disabled"
        );
        return;
    }
    info!("mirror audio receiver started");

    let cipher = Aes128::new(GenericArray::from_slice(&aeskey));
    let mut last_seq: Option<u16> = None;
    let mut buf = [0u8; 2048];
    let mut frames: u64 = 0;

    loop {
        let n = match tokio::time::timeout(IDLE_TIMEOUT, socket.recv(&mut buf)).await {
            Ok(Ok(n)) => n,
            Ok(Err(err)) => {
                debug!(?err, "mirror audio socket error");
                break;
            }
            Err(_) => {
                debug!(frames, "mirror audio idle timeout, ending");
                break;
            }
        };

        if n < 12 {
            continue;
        }
        let packet = &buf[..n];
        let payload = &packet[12..n];
        // Skip "no data" marker packets sent while the stream warms up.
        if payload.is_empty() || (payload.len() == 4 && payload == [0x00, 0x68, 0x34, 0x00]) {
            continue;
        }

        // De-duplicate the triple-send by sequence number.
        let seq = u16::from_be_bytes([packet[2], packet[3]]);
        if let Some(last) = last_seq
            && !seq_newer(seq, last)
        {
            continue;
        }
        last_seq = Some(seq);

        let frame = decrypt(&cipher, &aesiv, payload);
        if frame_tx.send(AudioFrame { data: frame }).is_err() {
            debug!("mirror audio receiver dropped, ending");
            break;
        }
        frames += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seq_newer_handles_wraparound() {
        assert!(seq_newer(5, 4));
        assert!(!seq_newer(4, 5));
        assert!(!seq_newer(4, 4)); // duplicate
        // wraparound: 1 is newer than 65535
        assert!(seq_newer(1, 0xffff));
        assert!(!seq_newer(0xffff, 1));
    }

    #[test]
    fn decrypt_leaves_partial_block_plaintext() {
        // 20-byte payload: 16 encrypted + 4 plaintext tail. With a known key/iv
        // the tail must pass through unchanged.
        let key = [0x11u8; 16];
        let iv = [0x22u8; 16];
        let cipher = Aes128::new(GenericArray::from_slice(&key));
        let payload: Vec<u8> = (0..20).collect();
        let out = decrypt(&cipher, &iv, &payload);
        assert_eq!(out.len(), 20);
        assert_eq!(&out[16..20], &payload[16..20], "tail preserved");
        assert_ne!(&out[0..16], &payload[0..16], "head decrypted");
    }
}
