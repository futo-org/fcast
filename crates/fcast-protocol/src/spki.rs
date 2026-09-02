//! Minimal DER walk extracting the SubjectPublicKeyInfo element from an X.509
//! certificate. The sender only needs the raw SPKI bytes for fingerprinting,
//! so a full X.509 parser dependency is avoided.

const TAG_SEQUENCE: u8 = 0x30;
const TAG_INTEGER: u8 = 0x02;
const TAG_CTX_0: u8 = 0xa0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpkiError {
    Truncated,
    UnexpectedTag { offset: usize, tag: u8 },
    BadLength,
}

impl std::fmt::Display for SpkiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SpkiError::Truncated => write!(f, "certificate DER is truncated"),
            SpkiError::UnexpectedTag { offset, tag } => {
                write!(f, "unexpected DER tag {tag:#04x} at offset {offset}")
            }
            SpkiError::BadLength => write!(f, "invalid DER length"),
        }
    }
}

impl std::error::Error for SpkiError {}

struct Cursor<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn byte(&mut self) -> Result<u8, SpkiError> {
        let b = *self.buf.get(self.pos).ok_or(SpkiError::Truncated)?;
        self.pos += 1;
        Ok(b)
    }

    fn peek(&self) -> Result<u8, SpkiError> {
        self.buf.get(self.pos).copied().ok_or(SpkiError::Truncated)
    }

    /// Read a TLV header and leave the cursor at the content. The returned
    /// content length is checked to fit in the buffer.
    fn header(&mut self) -> Result<(u8, usize), SpkiError> {
        let tag = self.byte()?;
        if tag & 0x1f == 0x1f {
            // High tag number form never appears in certificates
            return Err(SpkiError::UnexpectedTag {
                offset: self.pos - 1,
                tag,
            });
        }
        let first = self.byte()?;
        let len = match first {
            0x00..=0x7f => first as usize,
            // Indefinite length is not allowed in DER
            0x80 => return Err(SpkiError::BadLength),
            _ => {
                // Certificates never need length encodings above 4 bytes, and
                // the cap keeps the accumulation below from overflowing
                let n = (first & 0x7f) as usize;
                if n > 4 {
                    return Err(SpkiError::BadLength);
                }
                let mut len = 0usize;
                for i in 0..n {
                    let b = self.byte()?;
                    // DER requires minimal encoding, no leading zero bytes
                    if i == 0 && b == 0 {
                        return Err(SpkiError::BadLength);
                    }
                    len = (len << 8) | b as usize;
                }
                // A long form value below 0x80 must use the short form
                if len < 0x80 {
                    return Err(SpkiError::BadLength);
                }
                len
            }
        };
        if len > self.buf.len() - self.pos {
            return Err(SpkiError::Truncated);
        }
        Ok((tag, len))
    }

    /// Read a TLV header and require its tag.
    fn expect(&mut self, want: u8) -> Result<usize, SpkiError> {
        let offset = self.pos;
        let (tag, len) = self.header()?;
        if tag != want {
            return Err(SpkiError::UnexpectedTag { offset, tag });
        }
        Ok(len)
    }

    /// Skip a whole TLV, requiring its tag.
    fn skip(&mut self, want: u8) -> Result<(), SpkiError> {
        let len = self.expect(want)?;
        self.pos += len;
        Ok(())
    }
}

/// Extract the raw subjectPublicKeyInfo TLV (tag and length header included)
/// from a DER-encoded X.509 certificate. For well-formed DER this is byte
/// identical to x509-parser's `SubjectPublicKeyInfo::raw`. The SPKI content
/// itself is not parsed and nothing after it is validated. Bytes after the
/// certificate are ignored.
pub fn extract_spki(cert: &[u8]) -> Result<&[u8], SpkiError> {
    let mut cur = Cursor { buf: cert, pos: 0 };

    // Certificate ::= SEQUENCE { tbsCertificate, signatureAlgorithm, signature }
    let len = cur.expect(TAG_SEQUENCE)?;
    cur.buf = &cert[..cur.pos + len];

    // TBSCertificate ::= SEQUENCE, restrict the walk to its content
    let len = cur.expect(TAG_SEQUENCE)?;
    cur.buf = &cur.buf[..cur.pos + len];

    // version [0] EXPLICIT DEFAULT v1, absent in v1 certificates
    if cur.peek()? == TAG_CTX_0 {
        cur.skip(TAG_CTX_0)?;
    }
    cur.skip(TAG_INTEGER)?; // serialNumber
    cur.skip(TAG_SEQUENCE)?; // signature AlgorithmIdentifier
    cur.skip(TAG_SEQUENCE)?; // issuer
    cur.skip(TAG_SEQUENCE)?; // validity
    cur.skip(TAG_SEQUENCE)?; // subject

    // subjectPublicKeyInfo ::= SEQUENCE
    let start = cur.pos;
    let len = cur.expect(TAG_SEQUENCE)?;
    Ok(&cur.buf[start..cur.pos + len])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tlv(tag: u8, content: &[u8]) -> Vec<u8> {
        let mut out = vec![tag];
        let len = content.len();
        if len < 0x80 {
            out.push(len as u8);
        } else {
            let bytes = len.to_be_bytes();
            let skip = bytes.iter().take_while(|b| **b == 0).count();
            out.push(0x80 | (bytes.len() - skip) as u8);
            out.extend_from_slice(&bytes[skip..]);
        }
        out.extend_from_slice(content);
        out
    }

    /// Structurally valid certificate around the given SPKI TLV, no real
    /// crypto content.
    fn synthetic_cert(with_version: bool, spki: &[u8]) -> Vec<u8> {
        let mut tbs = Vec::new();
        if with_version {
            tbs.extend(tlv(TAG_CTX_0, &tlv(TAG_INTEGER, &[2])));
        }
        tbs.extend(tlv(TAG_INTEGER, &[1])); // serialNumber
        tbs.extend(tlv(TAG_SEQUENCE, &[])); // signature
        tbs.extend(tlv(TAG_SEQUENCE, &[])); // issuer
        tbs.extend(tlv(TAG_SEQUENCE, &[])); // validity
        tbs.extend(tlv(TAG_SEQUENCE, &[])); // subject
        tbs.extend_from_slice(spki);

        let mut cert = tlv(TAG_SEQUENCE, &tbs);
        cert.extend(tlv(TAG_SEQUENCE, &[])); // signatureAlgorithm
        cert.extend(tlv(0x03, &[0])); // signature BIT STRING
        tlv(TAG_SEQUENCE, &cert)
    }

    #[test]
    fn synthetic_v3() {
        let spki = tlv(TAG_SEQUENCE, b"not really a key");
        let cert = synthetic_cert(true, &spki);
        assert_eq!(extract_spki(&cert).unwrap(), &spki);
    }

    #[test]
    fn synthetic_v1_without_version_tag() {
        let spki = tlv(TAG_SEQUENCE, b"not really a key");
        let cert = synthetic_cert(false, &spki);
        assert_eq!(extract_spki(&cert).unwrap(), &spki);
    }

    #[test]
    fn long_form_lengths() {
        // One, two and three length bytes on the success path
        for content_len in [200usize, 300, 70_000] {
            let spki = tlv(TAG_SEQUENCE, &vec![0xab; content_len]);
            let cert = synthetic_cert(true, &spki);
            assert_eq!(extract_spki(&cert).unwrap(), &spki, "len {content_len}");
        }
    }

    #[test]
    fn trailing_bytes_ignored() {
        let spki = tlv(TAG_SEQUENCE, b"not really a key");
        let mut cert = synthetic_cert(true, &spki);
        cert.extend_from_slice(b"trailing garbage");
        assert_eq!(extract_spki(&cert).unwrap(), &spki);
    }

    fn rcgen_cert(alg: &'static rcgen::SignatureAlgorithm) -> (rcgen::KeyPair, Vec<u8>) {
        let key_pair = rcgen::KeyPair::generate_for(alg).unwrap();
        let cert = rcgen::CertificateParams::default()
            .self_signed(&key_pair)
            .unwrap();
        (key_pair, cert.der().to_vec())
    }

    #[test]
    fn differential_against_x509_parser_and_rcgen() {
        use rcgen::PublicKeyData;
        use x509_parser::prelude::{FromDer, X509Certificate};

        for alg in [
            &rcgen::PKCS_ECDSA_P256_SHA256,
            &rcgen::PKCS_ECDSA_P384_SHA384,
            &rcgen::PKCS_ED25519,
        ] {
            let (key_pair, der) = rcgen_cert(alg);
            let spki = extract_spki(&der).unwrap();
            let (_, parsed) = X509Certificate::from_der(&der).unwrap();
            assert_eq!(spki, parsed.tbs_certificate.subject_pki.raw);
            assert_eq!(spki, key_pair.subject_public_key_info());
        }
    }

    /// Self-signed RSA-2048 v3 certificate generated with openssl, CN "fcast
    /// spki test", covers a second DER producer.
    const RSA_CERT_DER: &[u8] = &[
        0x30, 0x82, 0x03, 0x17, 0x30, 0x82, 0x01, 0xff, 0xa0, 0x03, 0x02, 0x01, 0x02, 0x02, 0x14, 0x52,
        0xf3, 0x39, 0xf0, 0xf6, 0x65, 0xa3, 0x03, 0xff, 0xcd, 0xee, 0xda, 0x55, 0xb0, 0x6e, 0x90, 0x58,
        0xee, 0x94, 0x29, 0x30, 0x0d, 0x06, 0x09, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x0b,
        0x05, 0x00, 0x30, 0x1a, 0x31, 0x18, 0x30, 0x16, 0x06, 0x03, 0x55, 0x04, 0x03, 0x0c, 0x0f, 0x66,
        0x63, 0x61, 0x73, 0x74, 0x20, 0x73, 0x70, 0x6b, 0x69, 0x20, 0x74, 0x65, 0x73, 0x74, 0x30, 0x20,
        0x17, 0x0d, 0x32, 0x36, 0x30, 0x39, 0x30, 0x32, 0x31, 0x39, 0x35, 0x39, 0x30, 0x39, 0x5a, 0x18,
        0x0f, 0x32, 0x31, 0x32, 0x36, 0x30, 0x38, 0x30, 0x39, 0x31, 0x39, 0x35, 0x39, 0x30, 0x39, 0x5a,
        0x30, 0x1a, 0x31, 0x18, 0x30, 0x16, 0x06, 0x03, 0x55, 0x04, 0x03, 0x0c, 0x0f, 0x66, 0x63, 0x61,
        0x73, 0x74, 0x20, 0x73, 0x70, 0x6b, 0x69, 0x20, 0x74, 0x65, 0x73, 0x74, 0x30, 0x82, 0x01, 0x22,
        0x30, 0x0d, 0x06, 0x09, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x01, 0x05, 0x00, 0x03,
        0x82, 0x01, 0x0f, 0x00, 0x30, 0x82, 0x01, 0x0a, 0x02, 0x82, 0x01, 0x01, 0x00, 0xa4, 0x4a, 0x07,
        0xb7, 0x09, 0xff, 0xd0, 0x01, 0xc9, 0x70, 0x6f, 0xfd, 0xdd, 0x1d, 0x51, 0x95, 0xb1, 0x3f, 0x49,
        0x9a, 0x1e, 0x95, 0xf2, 0x62, 0xaf, 0xbe, 0x14, 0x69, 0xb6, 0x52, 0x91, 0x30, 0x9a, 0x03, 0x42,
        0xf4, 0x12, 0xbb, 0xa1, 0x15, 0xb5, 0xb3, 0x03, 0x5f, 0x1d, 0x93, 0x8a, 0xb4, 0x41, 0xdb, 0x65,
        0x81, 0xa4, 0xc1, 0x5b, 0xa6, 0x40, 0x5f, 0xa6, 0x4b, 0xa9, 0xb1, 0xda, 0xdf, 0x9c, 0x6f, 0x13,
        0x77, 0x43, 0x16, 0x62, 0xfa, 0x81, 0xfc, 0x26, 0x9f, 0xcc, 0x4d, 0xcd, 0xec, 0x51, 0x2c, 0xc3,
        0xf5, 0x92, 0x71, 0xe5, 0xc0, 0x56, 0x2d, 0xf2, 0x28, 0xfc, 0x41, 0x4b, 0xfb, 0xe2, 0xc1, 0x3d,
        0x52, 0x04, 0x3c, 0x78, 0xd1, 0x4f, 0x28, 0x81, 0xb2, 0xf8, 0x08, 0xea, 0xaf, 0x70, 0x4c, 0x92,
        0xf0, 0xf8, 0x8f, 0x4a, 0x52, 0xb3, 0x3c, 0x10, 0x60, 0x4c, 0xe7, 0x5a, 0x26, 0xab, 0x76, 0x3e,
        0x99, 0x2b, 0xbf, 0x8a, 0x94, 0xe7, 0x12, 0x14, 0xfe, 0xc4, 0x78, 0x4a, 0x54, 0x57, 0x1a, 0x51,
        0xf0, 0x44, 0x35, 0x0a, 0xbc, 0x57, 0x95, 0xe5, 0x5b, 0x52, 0xf1, 0x86, 0xe6, 0x69, 0x28, 0x8c,
        0xbe, 0x1c, 0xc5, 0xfb, 0x72, 0xe0, 0x85, 0x85, 0x8d, 0xfa, 0x10, 0xdf, 0xce, 0x99, 0xbc, 0x92,
        0x51, 0x49, 0xda, 0xb4, 0x71, 0x33, 0xb9, 0x33, 0x49, 0x12, 0x72, 0xba, 0x58, 0x5f, 0xf9, 0xc8,
        0x9c, 0xf3, 0x4f, 0x26, 0x86, 0x1c, 0xf4, 0x3c, 0xe7, 0x62, 0xb1, 0xb6, 0x60, 0xe4, 0xda, 0xd5,
        0xe0, 0x06, 0x1d, 0x05, 0x95, 0xb5, 0x07, 0xf6, 0x35, 0x4b, 0x04, 0x31, 0x97, 0xf0, 0x37, 0x2f,
        0x86, 0xc0, 0x55, 0xc6, 0x9d, 0xbf, 0x99, 0x9a, 0x2f, 0xc6, 0xe5, 0xe6, 0xf4, 0x0d, 0xc8, 0xf7,
        0x39, 0x86, 0x7d, 0xfc, 0x7a, 0xdd, 0xf9, 0x0c, 0xfe, 0xce, 0x43, 0xd7, 0x39, 0x02, 0x03, 0x01,
        0x00, 0x01, 0xa3, 0x53, 0x30, 0x51, 0x30, 0x1d, 0x06, 0x03, 0x55, 0x1d, 0x0e, 0x04, 0x16, 0x04,
        0x14, 0xea, 0xbb, 0xd6, 0xa8, 0xfa, 0xfb, 0xbc, 0x8d, 0x7f, 0xad, 0x4b, 0xad, 0x0a, 0x7b, 0x7b,
        0x17, 0x74, 0x6c, 0x96, 0xe4, 0x30, 0x1f, 0x06, 0x03, 0x55, 0x1d, 0x23, 0x04, 0x18, 0x30, 0x16,
        0x80, 0x14, 0xea, 0xbb, 0xd6, 0xa8, 0xfa, 0xfb, 0xbc, 0x8d, 0x7f, 0xad, 0x4b, 0xad, 0x0a, 0x7b,
        0x7b, 0x17, 0x74, 0x6c, 0x96, 0xe4, 0x30, 0x0f, 0x06, 0x03, 0x55, 0x1d, 0x13, 0x01, 0x01, 0xff,
        0x04, 0x05, 0x30, 0x03, 0x01, 0x01, 0xff, 0x30, 0x0d, 0x06, 0x09, 0x2a, 0x86, 0x48, 0x86, 0xf7,
        0x0d, 0x01, 0x01, 0x0b, 0x05, 0x00, 0x03, 0x82, 0x01, 0x01, 0x00, 0x1d, 0xa1, 0x58, 0xe0, 0xae,
        0xc7, 0xdd, 0xfb, 0x3d, 0x6b, 0xc1, 0x08, 0xf9, 0x5b, 0xde, 0xfc, 0x94, 0x95, 0x32, 0xff, 0xaa,
        0x3a, 0x7a, 0xb6, 0xed, 0x83, 0x93, 0xee, 0x8a, 0x7a, 0xfc, 0x01, 0x3f, 0xc5, 0x39, 0x5b, 0x9b,
        0xf5, 0x3a, 0x0c, 0xf0, 0xea, 0xed, 0xfc, 0x61, 0x6b, 0x27, 0x85, 0x0d, 0xb9, 0x25, 0x3c, 0x17,
        0x2f, 0x93, 0xb0, 0xc5, 0xe7, 0x3f, 0x70, 0x84, 0x4c, 0x2d, 0xa5, 0xac, 0x72, 0x0f, 0x0e, 0x4e,
        0x0a, 0xea, 0xc8, 0xc2, 0xa5, 0xf4, 0x9d, 0xac, 0x07, 0x1f, 0x48, 0x5a, 0x63, 0x7f, 0x11, 0x70,
        0x2c, 0x51, 0xf2, 0x9d, 0x0a, 0xae, 0x36, 0xcc, 0x41, 0x78, 0xd4, 0xca, 0x8e, 0x0b, 0x47, 0xcf,
        0xe2, 0x30, 0x87, 0x06, 0x96, 0x35, 0xb7, 0x1c, 0xe3, 0x29, 0x0f, 0x62, 0xa2, 0x26, 0xca, 0xf5,
        0x8f, 0x99, 0xc1, 0x09, 0xa1, 0x12, 0x79, 0x1f, 0xb8, 0xc7, 0x49, 0x12, 0x72, 0xb4, 0x3b, 0x6a,
        0x92, 0x5d, 0x03, 0xf0, 0x16, 0xf7, 0xf5, 0xb3, 0x41, 0xd3, 0xba, 0x02, 0x9f, 0xf1, 0x0e, 0x55,
        0xce, 0xe7, 0xa5, 0xba, 0x82, 0xa9, 0x4a, 0x8e, 0x63, 0x6c, 0x59, 0x4f, 0x4c, 0x6f, 0x71, 0x5b,
        0x42, 0x1c, 0x78, 0x85, 0x06, 0x88, 0x00, 0xfc, 0x48, 0xe7, 0x19, 0x6e, 0xa5, 0x51, 0xad, 0x7b,
        0x0a, 0x12, 0xc1, 0x6f, 0x93, 0xc4, 0xfb, 0xea, 0x58, 0x74, 0xe1, 0x0e, 0x14, 0x39, 0xb9, 0x31,
        0x40, 0x00, 0x02, 0x48, 0xc3, 0x24, 0x86, 0xe2, 0x91, 0xa8, 0xbe, 0xda, 0x0c, 0x9d, 0xcb, 0x95,
        0xb6, 0x7e, 0x53, 0xc4, 0xf2, 0x9c, 0x59, 0xba, 0xf8, 0x2a, 0x42, 0x32, 0x3c, 0x80, 0x51, 0x95,
        0x72, 0x34, 0x1f, 0x1a, 0xc6, 0x7d, 0x7a, 0xe5, 0x83, 0xb6, 0xe2, 0xe2, 0xf8, 0xf0, 0xa0, 0x05,
        0x8b, 0xb2, 0xaf, 0x5c, 0xef, 0x51, 0x92, 0xd4, 0xd9, 0x4b, 0x7a,
    ];

    #[test]
    fn differential_openssl_rsa_cert() {
        use x509_parser::prelude::{FromDer, X509Certificate};

        let spki = extract_spki(RSA_CERT_DER).unwrap();
        let (_, parsed) = X509Certificate::from_der(RSA_CERT_DER).unwrap();
        assert_eq!(spki, parsed.tbs_certificate.subject_pki.raw);
    }

    #[test]
    fn every_truncation_fails() {
        let (_, der) = rcgen_cert(&rcgen::PKCS_ECDSA_P256_SHA256);
        // The outer header length covers the whole certificate, so any
        // truncation fails while reading the outer header or at its bounds
        // check
        for len in 0..der.len() {
            assert!(extract_spki(&der[..len]).is_err(), "truncated to {len}");
        }
    }

    #[test]
    fn spki_claiming_past_tbs_end_fails() {
        // SPKI header declares 20 content bytes but only 5 remain in the TBS.
        // The outer certificate holds enough trailing bytes that an
        // unrestricted walk would return a wrong slice instead of an error.
        let mut tbs = Vec::new();
        tbs.extend(tlv(TAG_INTEGER, &[1]));
        tbs.extend(tlv(TAG_SEQUENCE, &[]));
        tbs.extend(tlv(TAG_SEQUENCE, &[]));
        tbs.extend(tlv(TAG_SEQUENCE, &[]));
        tbs.extend(tlv(TAG_SEQUENCE, &[]));
        tbs.extend_from_slice(&[TAG_SEQUENCE, 20, 1, 2, 3, 4, 5]);

        let mut cert = tlv(TAG_SEQUENCE, &tbs);
        cert.extend(tlv(TAG_SEQUENCE, &[0u8; 30])); // signatureAlgorithm
        let cert = tlv(TAG_SEQUENCE, &cert);
        assert_eq!(extract_spki(&cert), Err(SpkiError::Truncated));
    }

    #[test]
    fn tbs_claiming_past_outer_end_fails() {
        // TBS header declares 100 content bytes but the outer certificate
        // holds 10. The buffer carries enough trailing bytes that an
        // unrestricted walk would keep going.
        let mut outer = vec![TAG_SEQUENCE, 100];
        outer.extend_from_slice(&[0u8; 10]);
        let mut cert = tlv(TAG_SEQUENCE, &outer);
        cert.extend_from_slice(&[0u8; 200]);
        assert_eq!(extract_spki(&cert), Err(SpkiError::Truncated));
    }

    #[test]
    fn rejects_malformed() {
        assert_eq!(extract_spki(&[]), Err(SpkiError::Truncated));
        // Wrong outer tag
        assert!(matches!(
            extract_spki(&tlv(0x04, b"xx")),
            Err(SpkiError::UnexpectedTag { offset: 0, tag: 0x04 })
        ));
        // Indefinite length
        assert_eq!(
            extract_spki(&[0x30, 0x80, 0x00, 0x00]),
            Err(SpkiError::BadLength)
        );
        // Length encoding wider than 4 bytes
        assert_eq!(
            extract_spki(&[0x30, 0x85, 0x01, 0x01, 0x01, 0x01, 0x01]),
            Err(SpkiError::BadLength)
        );
        // Claimed length beyond the buffer
        assert_eq!(
            extract_spki(&[0x30, 0x84, 0xff, 0xff, 0xff, 0xff]),
            Err(SpkiError::Truncated)
        );
        // Non-minimal length encodings
        assert_eq!(
            extract_spki(&[0x30, 0x81, 0x05, 0, 0, 0, 0, 0]),
            Err(SpkiError::BadLength)
        );
        let mut cert = vec![0x30, 0x82, 0x00, 0x90];
        cert.extend_from_slice(&[0u8; 0x90]);
        assert_eq!(extract_spki(&cert), Err(SpkiError::BadLength));
        // High tag number form
        assert!(matches!(
            extract_spki(&[0x3f, 0x02, 0x00, 0x00]),
            Err(SpkiError::UnexpectedTag { .. })
        ));
    }

    #[test]
    fn garbage_never_panics() {
        let mut state = 0x243f6a8885a308d3u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for _ in 0..5000 {
            let len = (next() % 96) as usize;
            let buf: Vec<u8> = (0..len).map(|_| next() as u8).collect();
            let _ = extract_spki(&buf);
        }
    }

    #[test]
    fn mutated_real_cert_never_panics() {
        let (_, der) = rcgen_cert(&rcgen::PKCS_ECDSA_P256_SHA256);
        // Flip every byte through a few values, parse must never panic
        for i in 0..der.len() {
            for v in [0x00, 0x30, 0x7f, 0x80, 0x82, 0xa0, 0xff] {
                let mut mutated = der.clone();
                mutated[i] = v;
                let _ = extract_spki(&mutated);
            }
        }
    }
}
