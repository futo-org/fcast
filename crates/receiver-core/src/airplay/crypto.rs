//! Mirror-stream key derivation and the stateful AES-128-CTR cipher.
//!
//! The 16-byte AES key recovered from FairPlay (`aeskey_audio`) is only a seed:
//! the video key and IV are derived from it plus the per-session
//! `streamConnectionID`. The data stream is then one continuous AES-128-CTR
//! keystream spanning all packets, so the cipher state must persist across
//! them.

use aes::{
    Aes128,
    cipher::{KeyIvInit, StreamCipher},
};
use sha2::{Digest, Sha512};

/// Full-IV big-endian 128-bit counter - matches OpenSSL's `EVP_aes_128_ctr`.
type Aes128Ctr = ctr::Ctr128BE<Aes128>;

/// `key = SHA512("AirPlayStreamKey"+id || aeskey)[..16]`, and likewise for the
/// IV with the `"AirPlayStreamIV"` salt; the decimal id is appended as ASCII
/// with no null terminator.
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

/// AES-128-CTR whose keystream position is preserved across packets.
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

    /// Decrypt a packet payload in place, advancing the keystream. Equivalent
    /// to UxPlay's whole-block decrypt plus carried keystream tail: both
    /// amount to one contiguous CTR keystream over the concatenated
    /// payloads.
    pub fn decrypt(&mut self, buf: &mut [u8]) {
        self.cipher.apply_keystream(buf);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derives_expected_key_and_iv() {
        // Cross-checked against an independent SHA-512 computation.
        let aeskey: [u8; 16] = std::array::from_fn(|i| i as u8);
        let (key, iv) = derive_key_iv(&aeskey, 7654321);
        assert_eq!(hex(&key), "39a61b009deca64accd81e2907708142");
        assert_eq!(hex(&iv), "4397324ec3e4a12d39984f05d42efaa3");
    }

    #[test]
    fn ctr_keystream_is_contiguous_across_packets() {
        // Two packets decrypted separately must equal their concatenation.
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
