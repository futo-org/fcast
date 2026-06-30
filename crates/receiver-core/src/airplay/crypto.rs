//! Mirror-stream key derivation and the stateful AES-128-CTR cipher.
//!
//! The 16-byte AES key recovered from FairPlay (`aeskey_audio`) is only a seed:
//! the actual video cipher key and IV are derived from it together with the
//! per-session `streamConnectionID` (`mirror_buffer.c:init_aes`). The data stream
//! is then one continuous AES-128-CTR keystream spanning all packets - each
//! packet's ciphertext picks up exactly where the previous one left off, so the
//! cipher state must persist across packets (`mirror_buffer.c:decrypt`).

use aes::{
    Aes128,
    cipher::{KeyIvInit, StreamCipher},
};
use sha2::{Digest, Sha512};

/// Full-IV big-endian 128-bit counter - matches OpenSSL's `EVP_aes_128_ctr`.
type Aes128Ctr = ctr::Ctr128BE<Aes128>;

/// Derive the mirror video key and IV from the FairPlay audio key and the
/// session's `streamConnectionID`.
///
/// `key = SHA512("AirPlayStreamKey"+id || aeskey)[..16]`, and likewise for the IV
/// with the `"AirPlayStreamIV"` salt. The decimal id is appended as ASCII with
/// no null terminator.
fn derive_key_iv(aeskey_audio: &[u8; 16], stream_connection_id: u64) -> ([u8; 16], [u8; 16]) {
    let derive = |salt: &str| -> [u8; 16] {
        let mut hasher = Sha512::new();
        hasher.update(format!("{salt}{stream_connection_id}").as_bytes());
        hasher.update(aeskey_audio);
        let digest = hasher.finalize();
        let mut out = [0u8; 16];
        out.copy_from_slice(&digest[..16]);
        out
    };
    (derive("AirPlayStreamKey"), derive("AirPlayStreamIV"))
}

/// The mirror video cipher: AES-128-CTR whose keystream position is preserved
/// across packets.
pub struct MirrorCipher {
    cipher: Aes128Ctr,
}

impl MirrorCipher {
    pub fn new(aeskey_audio: &[u8; 16], stream_connection_id: u64) -> Self {
        let (key, iv) = derive_key_iv(aeskey_audio, stream_connection_id);
        Self {
            cipher: Aes128Ctr::new(&key.into(), &iv.into()),
        }
    }

    /// Decrypt a packet payload in place, advancing the keystream.
    ///
    /// UxPlay decrypts only whole 16-byte blocks per packet and carries the
    /// leftover keystream tail into the next packet. Because the carried tail
    /// always completes the partial block before the next packet's whole-block
    /// region begins, the net effect is a single contiguous CTR keystream over
    /// the concatenation of all payloads - which is exactly what advancing one
    /// stateful CTR cipher by `buf.len()` bytes produces.
    pub fn decrypt(&mut self, buf: &mut [u8]) {
        self.cipher.apply_keystream(buf);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derives_expected_key_and_iv() {
        // Cross-checked against an independent SHA-512 computation (see the
        // milestone-3 derivation): aeskey = 00..0f, id = 7654321.
        let aeskey: [u8; 16] = std::array::from_fn(|i| i as u8);
        let (key, iv) = derive_key_iv(&aeskey, 7654321);
        assert_eq!(hex(&key), "39a61b009deca64accd81e2907708142");
        assert_eq!(hex(&iv), "4397324ec3e4a12d39984f05d42efaa3");
    }

    #[test]
    fn ctr_keystream_is_contiguous_across_packets() {
        // Decrypting two packets separately must equal decrypting their
        // concatenation in one shot - the property the cross-packet carry in
        // mirror_buffer.c relies on.
        let aeskey: [u8; 16] = std::array::from_fn(|i| (0xa0 + i) as u8);

        let plaintext: Vec<u8> = (0..50u16).map(|i| i as u8).collect();

        // Split decrypt at a non-block-aligned boundary (19 bytes).
        let mut split = plaintext.clone();
        let mut c = MirrorCipher::new(&aeskey, 42);
        let (a, b) = split.split_at_mut(19);
        c.decrypt(a);
        c.decrypt(b);

        let mut whole = plaintext.clone();
        MirrorCipher::new(&aeskey, 42).decrypt(&mut whole);

        assert_eq!(split, whole);
    }

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }
}
