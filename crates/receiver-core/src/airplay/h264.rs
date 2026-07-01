//! H.264 framing for the mirror stream: convert Apple's length-prefixed NAL
//! units (avcC-style) into Annex-B byte-stream that a GStreamer `h264parse` +
//! decoder accepts.
//!
//! Two payload shapes occur (`raop_rtp_mirror.c`):
//! - **Video** (`0x00`/`0x10`, decrypted): a sequence of NAL units each prefixed
//!   by a 4-byte big-endian length. We replace every length prefix with the
//!   `00 00 00 01` start code in place.
//! - **Config** (`0x01`, unencrypted): an avcC-like record holding one SPS and
//!   one PPS. We emit them as two Annex-B NAL units. These are sent in their own
//!   packet and must be prepended to the next (IDR) video frame.

/// Annex-B NAL unit start code.
const START_CODE: [u8; 4] = [0x00, 0x00, 0x00, 0x01];

/// Rewrite a decrypted video payload's 4-byte big-endian NAL length prefixes
/// into Annex-B start codes, in place. Returns `false` (leaving `buf` partially
/// rewritten) if the length fields don't tile the buffer exactly.
pub fn length_prefixed_to_annex_b(buf: &mut [u8]) -> bool {
    let len = buf.len();
    let mut i = 0;
    while i + 4 <= len {
        let nal_len = u32::from_be_bytes([buf[i], buf[i + 1], buf[i + 2], buf[i + 3]]) as usize;
        if i + 4 + nal_len > len {
            return false;
        }
        buf[i..i + 4].copy_from_slice(&START_CODE);
        i += 4 + nal_len;
    }
    i == len
}

/// Parse an H.264 config (`0x01`) payload - an avcC-style SPS/PPS record - into
/// Annex-B (`start_code || SPS || start_code || PPS`). Returns `None` if the record
/// is malformed or too short (e.g. an H.265 `hvc1` payload, which we don't
/// support).
///
/// Layout (`raop_rtp_mirror.c`): `sps_size = BE16(payload[6])`, SPS at
/// `payload[8]`; `pps_size = BE16(payload[sps_size + 9])`, PPS at
/// `payload[sps_size + 11]`.
pub fn config_to_annex_b(payload: &[u8]) -> Option<Vec<u8>> {
    // Need at least the avcC header + the SPS length field.
    if payload.len() < 8 {
        return None;
    }
    let sps_size = u16::from_be_bytes([payload[6], payload[7]]) as usize;
    let sps_start = 8;
    let pps_len_off = sps_size + 9;
    // Room for the SPS and the 2-byte PPS length field that follows it.
    if pps_len_off + 2 > payload.len() {
        return None;
    }
    let pps_size = u16::from_be_bytes([payload[pps_len_off], payload[pps_len_off + 1]]) as usize;
    let pps_start = sps_size + 11;
    if pps_start + pps_size > payload.len() {
        return None;
    }

    let mut out = Vec::with_capacity(sps_size + pps_size + 8);
    out.extend_from_slice(&START_CODE);
    out.extend_from_slice(&payload[sps_start..sps_start + sps_size]);
    out.extend_from_slice(&START_CODE);
    out.extend_from_slice(&payload[pps_start..pps_start + pps_size]);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a length-prefixed payload from NAL unit bodies.
    fn length_prefixed(nals: &[&[u8]]) -> Vec<u8> {
        let mut v = Vec::new();
        for nal in nals {
            v.extend_from_slice(&(nal.len() as u32).to_be_bytes());
            v.extend_from_slice(nal);
        }
        v
    }

    #[test]
    fn rewrites_single_nal() {
        let mut buf = length_prefixed(&[&[0x65, 0xaa, 0xbb]]);
        assert!(length_prefixed_to_annex_b(&mut buf));
        assert_eq!(buf, [0x00, 0x00, 0x00, 0x01, 0x65, 0xaa, 0xbb]);
    }

    #[test]
    fn rewrites_multiple_nals() {
        let mut buf = length_prefixed(&[&[0x67, 0x01], &[0x68], &[0x65, 0x02, 0x03, 0x04]]);
        assert!(length_prefixed_to_annex_b(&mut buf));
        assert_eq!(
            buf,
            [
                0x00, 0x00, 0x00, 0x01, 0x67, 0x01, // NAL 1
                0x00, 0x00, 0x00, 0x01, 0x68, // NAL 2
                0x00, 0x00, 0x00, 0x01, 0x65, 0x02, 0x03, 0x04, // NAL 3
            ]
        );
    }

    #[test]
    fn rejects_overrunning_length() {
        // Claims a 100-byte NAL but only 2 bytes follow.
        let mut buf = vec![0x00, 0x00, 0x00, 0x64, 0xaa, 0xbb];
        assert!(!length_prefixed_to_annex_b(&mut buf));
    }

    #[test]
    fn rejects_trailing_garbage() {
        // One valid 1-byte NAL, then 3 leftover bytes that can't form a header.
        let mut buf = vec![0x00, 0x00, 0x00, 0x01, 0x65, 0x11, 0x22, 0x33];
        assert!(!length_prefixed_to_annex_b(&mut buf));
    }

    #[test]
    fn parses_config_sps_pps() {
        let sps = [0x67, 0x42, 0x00, 0x0a];
        let pps = [0x68, 0xce, 0x3c, 0x80];
        // avcC-style header: 6 bytes, then BE16 sps_size, SPS, num_pps byte,
        // BE16 pps_size, PPS.
        let mut payload = vec![0x01, 0x42, 0x00, 0x0a, 0xff, 0xe1];
        payload.extend_from_slice(&(sps.len() as u16).to_be_bytes());
        payload.extend_from_slice(&sps);
        payload.push(0x01); // number of PPS
        payload.extend_from_slice(&(pps.len() as u16).to_be_bytes());
        payload.extend_from_slice(&pps);

        let annex_b = config_to_annex_b(&payload).expect("valid config");
        let mut expected = Vec::new();
        expected.extend_from_slice(&START_CODE);
        expected.extend_from_slice(&sps);
        expected.extend_from_slice(&START_CODE);
        expected.extend_from_slice(&pps);
        assert_eq!(annex_b, expected);
    }

    #[test]
    fn rejects_short_config() {
        assert!(config_to_annex_b(&[0x01, 0x42, 0x00]).is_none());
    }
}
