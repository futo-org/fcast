mod srp_client;
mod tlv8;

use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::Duration,
};

use aead::{Aead, KeyInit, Payload};
use anyhow::{anyhow, bail, ensure, Context};
use chacha20poly1305::ChaCha20Poly1305;
use ed25519_dalek::ed25519::signature::Signer;
use ed25519_dalek::Verifier;
use hex_literal::hex;
use log::{debug, error, info};
use num_bigint::BigUint;
use plist::PlistParseError;
use rand_pcg::rand_core::RngCore;
use rtsp::StatusCode;
use sha2::Sha512;
use srp_client::SrpClient;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    runtime::Handle,
    sync::mpsc::{Receiver, Sender},
};

use crate::{
    airplay_common::{self, AirPlayFeatures, AirPlayStatus, InfoPlist},
    casting_device::{
        CastingDevice, CastingDeviceError, DeviceConnectionState, DeviceEventHandler,
        DeviceFeature, DeviceInfo, GenericEventSubscriptionGroup, ProtocolType,
    },
    utils, IpAddr,
};

use parsers_common::{find_first_cr_lf, find_first_double_cr_lf, parse_header_map};

const DEVICE_ID: &str = "C9635ED0964902E0";
const PAIRING_USERNAME: &str = "Pair-Setup";
const TAG_LENGTH: usize = 16; // Poly1305
const MAX_BLOCK_LENGTH: usize = 0x400;
const BLOCK_LENGTH_LENGTH: usize = 2; // u16
const FEEDBACK_INTERVAL: Duration = Duration::from_secs(2); // https://github.com/postlund/pyatv/blob/49f9c9e960930276c8fac9fb7696b54a7beb1951/pyatv/protocols/raop/protocols/airplayv2.py#L25

/// The Modulus, N, and Generator, g, are specified by the 3072-bit group of [RFC 5054](https://tools.ietf.org/html/rfc5054).
fn srp_group_n() -> BigUint {
    BigUint::from_bytes_be(&hex!(
        "FFFFFFFF FFFFFFFF C90FDAA2 2168C234 C4C6628B 80DC1CD1 29024E08 8A67CC74 \
        020BBEA6 3B139B22 514A0879 8E3404DD EF9519B3 CD3A431B 302B0A6D F25F1437 \
        4FE1356D 6D51C245 E485B576 625E7EC6 F44C42E9 A637ED6B 0BFF5CB6 F406B7ED \
        EE386BFB 5A899FA5 AE9F2411 7C4B1FE6 49286651 ECE45B3D C2007CB8 A163BF05 \
        98DA4836 1C55D39A 69163FA8 FD24CF5F 83655D23 DCA3AD96 1C62F356 208552BB \
        9ED52907 7096966D 670C354E 4ABC9804 F1746C08 CA18217C 32905E46 2E36CE3B \
        E39E772C 180E8603 9B2783A2 EC07A28F B5C55DF0 6F4C52C9 DE2BCBF6 95581718 \
        3995497C EA956AE5 15D22618 98FA0510 15728E5A 8AAAC42D AD33170D 04507A33 \
        A85521AB DF1CBA64 ECFB8504 58DBEF0A 8AEA7157 5D060C7D B3970F85 A6E1E4C7 \
        ABF5AE8C DB0933D7 1E8C94E0 4A25619D CEE3D226 1AD2EE6B F12FFA06 D98A0864 \
        D8760273 3EC86A64 521F2B18 177B200C BBE11757 7A615D6C 770988C0 BAD946E2 \
        08E24FA0 74E5AB31 43DB5BFC E0FD108E 4B82D120 A93AD2CA FFFFFFFF FFFFFFFF"
    ))
}

pub fn info_from_plist(plist: &[u8]) -> Result<InfoPlist, PlistParseError> {
    let mut reader = plist::PlistReader::new(plist);
    reader.read_magic_number()?;
    reader.read_version()?;
    let trailer = reader.read_trailer()?;
    let mut info = InfoPlist::default();
    let mut parser = plist::PlistParser::new(plist, trailer)?;
    let parsed = parser.parse()?;
    for obj in parsed {
        if let plist::Object::Dict(items) = obj {
            for (key, val) in items {
                let plist::Object::String(key) = key else {
                    continue;
                };
                match key.as_str() {
                    "features" => {
                        if let plist::Object::Int(features) = val {
                            info.features =
                                Some(AirPlayFeatures::from_bits_truncate(features as u64));
                        }
                    }
                    "statusFlags" => {
                        if let plist::Object::Int(status_flags) = val {
                            info.status_flags =
                                Some(AirPlayStatus::from_bits_truncate(status_flags as u32));
                        }
                    }
                    _ => (),
                }
            }
        }
    }

    Ok(info)
}

fn srp_group_g() -> BigUint {
    BigUint::from_bytes_be(&hex!("05"))
}

fn hkdf_extract_expand(ikm: &[u8], salt: &[u8], info: &[u8], okm: &mut [u8]) -> anyhow::Result<()> {
    let hkdf = hkdf::Hkdf::<Sha512>::new(Some(salt), ikm);
    hkdf.expand(info, okm)
        .map_err(|err| anyhow!("failed to expand: {err}"))
}

fn chacha20_poly1305_encrypt(
    key: &[u8; 32],
    nonce: &[u8; 12],
    aad: &[u8],
    plaintext: &[u8],
) -> anyhow::Result<(Vec<u8>, Vec<u8>)> {
    let cipher = ChaCha20Poly1305::new(key.into());
    let payload = Payload {
        msg: plaintext,
        aad,
    };
    let ciphertext = cipher
        .encrypt(nonce.into(), payload)
        .map_err(|err| anyhow!("failed to encrypt: {err}"))?;
    Ok((
        ciphertext[..plaintext.len()].to_vec(),
        ciphertext[plaintext.len()..].to_vec(),
    ))
}

fn chacha20_poly1305_decrypt(
    key: &[u8; 32],
    nonce: &[u8; 12],
    aad: &[u8],
    ciphertext: &[u8],
    mac: &[u8],
) -> anyhow::Result<Vec<u8>> {
    let cipher = ChaCha20Poly1305::new(key.into());
    let payload = Payload {
        msg: &[ciphertext, mac].concat(),
        aad,
    };
    // debug!("key: {key:?}");
    // debug!("nonce: {nonce:?}");
    // debug!("aad: {aad:?}");
    // debug!("ciphertext: {ciphertext:?}");
    // debug!("mac: {mac:?}");
    cipher
        .decrypt(nonce.into(), payload)
        .map_err(|err| anyhow!("failed to decrypt: {err}"))
}

#[derive(Debug, PartialEq)]
enum Command {
    Quit,
}

#[derive(Debug, PartialEq)]
#[repr(u8)]
enum PairingState {
    M1 = 1,
    M2 = 2,
    M3 = 3,
    M4 = 4,
    M5 = 5,
    M6 = 6,
}

#[derive(Debug, thiserror::Error)]
enum PairingStateError {
    #[error("Invalid value `{0}`")]
    InvalidValue(u8),
}

impl TryFrom<u8> for PairingState {
    type Error = PairingStateError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        Ok(match value {
            1 => Self::M1,
            2 => Self::M2,
            3 => Self::M3,
            4 => Self::M4,
            5 => Self::M5,
            6 => Self::M6,
            _ => return Err(PairingStateError::InvalidValue(value)),
        })
    }
}

#[allow(dead_code)]
#[repr(u8)]
enum PairingMethod {
    PairSetup = 0,
    PairSetupWithAuth = 1,
    PairVerify = 2,
    AddPairing = 3,
    RemovePairing = 4,
    ListPairings = 5,
}

#[derive(Debug)]
enum SenderState {
    WaitingOnPairSetup1,
    WaitingOnPairSetup2,
    WaitingOnPairSetup3,
    WaitingOnPairVerify1,
    WaitingOnPairVerify2,
    ReadyToPlay,
}

struct InnerDevice {
    event_handler: Arc<dyn DeviceEventHandler>,
    sender_state: SenderState,
    srp_client: Option<SrpClient>,
    used_remote_addr: Option<SocketAddr>,
    password: Option<String>,
    session_key: Option<Vec<u8>>,
    device_private_key: Option<[u8; 32]>,
    device_public_key: Option<[u8; 32]>,
    accessory_ltpk: Option<Vec<u8>>,
    verifier_private_key: Option<x25519_dalek::EphemeralSecret>,
    verifier_public_key: Option<x25519_dalek::PublicKey>,
    accessory_curve_public: Option<Vec<u8>>,
    accessory_shared_secret: Option<x25519_dalek::SharedSecret>,
    outgoing_key: [u8; 32], // TODO: Option<>
    incoming_key: [u8; 32], // TODO: Option<>
    is_encrypted: bool,
    paired: bool,
    out_count: u64,
    in_count: u64,
    stream: Option<TcpStream>,
    rtsp_cseq: usize,
    // TODO: use a pool
    scratch_buffer: Vec<u8>,
}

impl InnerDevice {
    pub fn new(event_handler: Arc<dyn DeviceEventHandler>) -> Self {
        Self {
            event_handler,
            sender_state: SenderState::WaitingOnPairSetup1,
            srp_client: None,
            used_remote_addr: None,
            password: None,
            session_key: None,
            device_private_key: None,
            device_public_key: None,
            accessory_ltpk: None,
            verifier_private_key: None,
            verifier_public_key: None,
            accessory_curve_public: None,
            accessory_shared_secret: None,
            outgoing_key: [0; 32],
            incoming_key: [0; 32],
            is_encrypted: false,
            paired: false,
            out_count: 0,
            in_count: 0,
            stream: None,
            rtsp_cseq: 1,
            scratch_buffer: Vec::new(),
        }
    }

    async fn send_rtsp_request(
        &mut self,
        req: rtsp::Request<'_>,
    ) -> anyhow::Result<(StatusCode, Option<Vec<u8>>)> {
        let Some(stream) = self.stream.as_mut() else {
            bail!("Cannot send request because stream is missing");
        };

        self.scratch_buffer.clear();
        req.encode_into(&mut self.scratch_buffer);
        stream.write_all(&self.scratch_buffer).await?;

        // TODO: make sure we return the right response for the CSeq sent now

        self.scratch_buffer.clear();
        let read_buf = &mut self.scratch_buffer;
        let mut tmp_buf = [0u8; 1024];

        loop {
            let read_bytes = stream.read(&mut tmp_buf).await?;
            read_buf.extend_from_slice(&tmp_buf[0..read_bytes]);
            if read_bytes < tmp_buf.len() {
                break;
            }
        }

        let status_line_end =
            find_first_cr_lf(read_buf).ok_or(anyhow!("No CR LF found for statusline"))? + 2;

        let status = rtsp::parse_response_statusline(&read_buf[0..status_line_end])?;

        let headers_end = 'out: loop {
            match find_first_double_cr_lf(read_buf) {
                Some(end) => break 'out end + 4,
                None => {
                    debug!("No trailing CR LF found after header map, trying to read more");
                    let read_bytes = stream.read(&mut tmp_buf).await?;
                    if read_bytes == 0 {
                        bail!("Malformed response (missing trailing CR LF sequence in header map)");
                    }
                    read_buf.extend_from_slice(&tmp_buf[0..read_bytes]);
                }
            }
        };

        let headers = parse_header_map(&read_buf[status_line_end..headers_end])?;
        let mut content_length = None::<usize>;
        for header in headers {
            if header.0 == b"Content-Length" {
                let value_str = String::from_utf8_lossy(header.1);
                match value_str.parse::<usize>() {
                    Ok(len) => content_length = Some(len),
                    Err(err) => error!("Failed to parse Content-Length as usize: {err}"),
                }
            }
            debug!(
                "Response header: {}: {}",
                String::from_utf8_lossy(header.0),
                String::from_utf8_lossy(header.1)
            );
        }

        let mut body = None::<Vec<u8>>;
        if let Some(content_length) = content_length {
            while read_buf.len() < headers_end + content_length {
                let read_bytes = stream.read(&mut tmp_buf).await?;
                if read_bytes == 0 {
                    bail!(
                        "Failed to read body, {} bytes are missing",
                        content_length - read_buf.len() - headers_end
                    );
                }
                read_buf.extend_from_slice(&tmp_buf[0..read_bytes]);
            }
            debug!("--- Response ---\n{}", hexdump::hexdump(read_buf));
            body = Some(read_buf[headers_end..headers_end + content_length].to_vec());
        }

        Ok((status, body))
    }

    async fn post_rtsp_with_resp(
        &mut self,
        path: &str,
        body_bytes: &[u8],
        headers: Option<&[(&str, &str)]>,
    ) -> anyhow::Result<(StatusCode, Option<Vec<u8>>)> {
        let full_path = format!("/{path}");
        let cseq_str = self.rtsp_cseq.to_string();
        self.rtsp_cseq += 1;

        let mut req_headers = vec![
            ("User-Agent", "AirPlay/381.13"),
            ("X-Apple-HKP", "3"),
            ("X-Apple-Client-Name", "FCast Sender SDK"),
            ("CSeq", &cseq_str),
        ];

        if let Some(provided_headers) = headers {
            req_headers.extend_from_slice(provided_headers);
        }

        let req = rtsp::Request {
            method: rtsp::Method::Post,
            path: &full_path,
            version: rtsp::Version::Rtsp10,
            headers: &req_headers,
            body: Some(body_bytes),
        };

        debug!("Sending POST request: {req:?}");

        self.send_rtsp_request(req).await
    }

    async fn send_feedback(&mut self) -> anyhow::Result<()> {
        let cseq_str = self.rtsp_cseq.to_string();
        self.rtsp_cseq += 1;
        let req = rtsp::Request {
            method: rtsp::Method::Post,
            path: "/feedback",
            version: rtsp::Version::Rtsp10,
            headers: &[("Content-Length", "0"), ("CSeq", &cseq_str)],
            body: None,
        };

        debug!("Sennding request: {req:?}");

        let mut encoded_req = Vec::new();
        req.encode_into(&mut encoded_req);

        let encrypted = self.encrypt_data(&encoded_req)?;

        match self.stream.as_mut() {
            Some(stream) => stream.write_all(&encrypted).await?,
            None => bail!("Cannot send feedback because stream is missing"),
        }

        let mut read_buf = Vec::new();
        let mut tmp_buf = [0u8; 1024];
        let plaintext = loop {
            match self.stream.as_mut() {
                Some(stream) => {
                    if stream.read(&mut tmp_buf).await? == 0 {
                        bail!("No more data to read");
                    }
                }
                None => bail!("Cannot send feedback because stream is missing"),
            }
            read_buf.extend_from_slice(&tmp_buf);
            if let Some(plaintext) = self.decrypt_data(&read_buf)? {
                break plaintext;
            }
        };

        debug!("Response:\n{}", hexdump::hexdump(&plaintext));

        Ok(())
    }

    // M1: iOS Device -> Accessory – 'SRP Start Request'
    async fn pair_setup_m1_m2(&mut self, pin: Option<&str>) -> anyhow::Result<Vec<u8>> {
        self.sender_state = SenderState::WaitingOnPairSetup1;

        let password = pin.unwrap_or("3939");

        self.password = Some(password.to_owned());

        self.srp_client = Some(SrpClient::new(
            srp_group_n(),
            srp_group_g(),
            PAIRING_USERNAME.as_bytes().to_vec(),
            password.as_bytes().to_vec(),
        ));

        // When the iOS device performs authentication as part of the Pair Setup procedure, it sends
        // a request to the accessory with the following TLV items:
        //     kTLVType_State <M1>
        //     kTLVType_Method <Pair Setup with Authentication>
        let state_item = tlv8::Item::new(tlv8::Tag::State, vec![PairingState::M1 as u8]);
        let method_item = tlv8::Item::new(
            tlv8::Tag::Method,
            vec![PairingMethod::PairSetupWithAuth as u8],
        );
        let encoded_tlv = tlv8::encode(&[state_item, method_item], false);
        let encoded_tlv_len_str = encoded_tlv.len().to_string();

        // When the iOS device performs Pair Setup with a separate optional authentication procedure,
        // it sends a request to the accessory with the following TLV items:
        //     kTLVType_State <M1>
        //     kTLVType_Method <Pair Setup>
        //     kTLVType_Flags <Pairing Type Flags>

        let headers = [
            ("Content-Type", "application/octet-stream"),
            ("Content-Length", &encoded_tlv_len_str),
        ];

        debug!("pair-setup [1/5]: sending request...");

        let (resp_status, resp_body) = self
            .post_rtsp_with_resp("pair-setup", &encoded_tlv, Some(&headers))
            .await?;

        if resp_status == rtsp::StatusCode::Ok {
            let Some(body) = resp_body else {
                bail!("pair-setup [1/5] failed: missing body");
            };
            Ok(body)
        } else {
            Err(anyhow!("Failed to initiate pair-setup"))
        }
    }

    // M3: iOS Device -> Accessory – 'SRP Verify Request'
    async fn pair_setup_m2_m3(
        &mut self,
        fields: HashMap<tlv8::Tag, Vec<u8>>,
    ) -> anyhow::Result<Vec<u8>> {
        self.sender_state = SenderState::WaitingOnPairSetup2;

        let Some(salt_bytes) = fields.get(&tlv8::Tag::Salt) else {
            bail!("Missing salt");
        };
        let Some(b_bytes) = fields.get(&tlv8::Tag::PublicKey) else {
            bail!("Missing public key");
        };

        let Some(client) = self.srp_client.as_mut() else {
            bail!("SRP client is not initialized");
        };

        // 4. Generate its SRP public key with SRP_gen_pub().
        debug!("Starting authentication");
        let a_pub = client.srp_user_start_authentication(None)?;

        // 3. Set salt provided by the accessory in the <M2> TLV with SRP_set_params().
        // 6. Compute the SRP shared secret key with SRP_compute_key().
        // 7. Generate iOS device-side SRP proof with SRP_respond().
        debug!("pair-setup [2/5]: Calculating m1");
        let m1_bytes = client.srp_user_process_challenge(salt_bytes, b_bytes)?;

        // Send a request to the accessory with the following TLV items:
        //     kTLVType_State <M3>
        //     kTLVType_PublicKey <iOS device’s SRP public key>
        //     kTLVType_Proof <iOS device’s SRP proof>
        let state_item = tlv8::Item::new(tlv8::Tag::State, vec![PairingState::M3 as u8]);
        let pk_item = tlv8::Item::new(tlv8::Tag::PublicKey, a_pub.to_bytes_be());
        let proof_item = tlv8::Item::new(tlv8::Tag::Proof, m1_bytes.to_vec());
        let encoded_tlv = tlv8::encode(&[state_item, pk_item, proof_item], false);
        let encodec_tlv_len_str = encoded_tlv.len().to_string();

        let headers = [
            ("Content-Type", "application/octet-stream"),
            ("Content-Length", &encodec_tlv_len_str),
        ];

        debug!("pair-setup [3/5]: sending request...");

        let (resp_status, resp_body) = self
            .post_rtsp_with_resp("pair-setup", &encoded_tlv, Some(&headers))
            .await?;

        if resp_status == rtsp::StatusCode::Ok {
            let Some(body) = resp_body else {
                bail!("M2 -> M3 failed: missing body");
            };
            Ok(body)
        } else {
            Err(anyhow!("Failed to initiate pair-setup"))
        }
    }

    // M5: iOS Device -> Accessory – 'Exchange Request'
    async fn pair_setup_m4_m5(
        &mut self,
        fields: HashMap<tlv8::Tag, Vec<u8>>,
    ) -> anyhow::Result<Vec<u8>> {
        self.sender_state = SenderState::WaitingOnPairSetup3;

        let Some(server_proof_bytes) = fields.get(&tlv8::Tag::Proof) else {
            bail!("Proof is missing");
        };

        let Some(srp_client) = self.srp_client.as_ref() else {
            bail!("SRP client is not initialized");
        };

        // Verify accessoryʼs SRP proof with SRP_verify(). If this fails, the setup process will be
        // aborted and an error will be reported to the user
        debug!("pair-setup [4/5]: verifying...");
        if !srp_client.user_verify_session(server_proof_bytes)? {
            bail!("Server authentication failed");
        }

        let session_key = srp_client
            .get_session_key()
            .ok_or(anyhow!("Missing session key"))?;
        self.session_key = Some(session_key.clone());

        // Generate its Ed25519 long-term public key, iOSDeviceLTPK, and long-term secret key, iOSDeviceLTSK.
        let mut rng = rand_pcg::Pcg64::new(0xcafef00dd15ea5e5, 0xa02bdbf7bb3c0a7);
        let mut seed = [0u8; 32];
        rng.fill_bytes(&mut seed);
        let ed_priv = ed25519_dalek::SigningKey::from_bytes(&seed);
        let ed_pub = ed_priv.verifying_key().to_bytes();
        // self.device_private_key = Some(ed_priv.to_keypair_bytes().to_bytes());
        self.device_private_key = Some(ed_priv.to_bytes());
        self.device_public_key = Some(ed_pub);

        let mut device_x = [0u8; 32];
        hkdf_extract_expand(
            &session_key,
            b"Pair-Setup-Controller-Sign-Salt",
            b"Pair-Setup-Controller-Sign-Info",
            &mut device_x,
        )?;

        // Concatenate iOSDeviceX with the iOS deviceʼs Pairing Identifier, iOSDevicePairingID, and its long-term
        // public key, iOSDeviceLTPK. The data must be concatenated in order such that the final data is iOSDeviceX,
        // iOSDevicePairingID, iOSDeviceLTPK. The concatenated value will be referred to as iOSDeviceInfo.
        self.scratch_buffer.clear();
        let device_info = &mut self.scratch_buffer;
        device_info.extend_from_slice(&device_x);
        device_info.extend_from_slice(DEVICE_ID.as_bytes());
        device_info.extend_from_slice(&ed_pub);
        // Generate iOSDeviceSignature by signing iOSDeviceInfo with its long-term secret key, iOSDeviceLTSK,
        // using Ed25519.
        let signature = ed_priv.sign(device_info).to_bytes();

        // Construct a sub-TLV with the following TLV items:
        //     kTLVType_Identifier <iOSDevicePairingID>
        //     kTLVType_PublicKey <iOSDeviceLTPK>
        //     kTLVType_Signature <iOSDeviceSignature>
        let identifier_item = tlv8::Item::new(tlv8::Tag::Identifier, DEVICE_ID.as_bytes().to_vec());
        let public_key_item = tlv8::Item::new(tlv8::Tag::PublicKey, ed_pub.to_vec());
        let sig_item = tlv8::Item::new(tlv8::Tag::Signature, signature.to_vec());
        let sub_tlv = tlv8::encode(&[identifier_item, public_key_item, sig_item], false);

        let mut session_key_2 = [0u8; 32];
        hkdf_extract_expand(
            &session_key,
            "Pair-Setup-Encrypt-Salt".as_bytes(),
            "Pair-Setup-Encrypt-Info".as_bytes(),
            &mut session_key_2,
        )?;

        // Encrypt the sub-TLV, encryptedData, and generate the 16 byte auth tag, authTag. This uses the
        // ChaCha20-Poly1305 AEAD algorithm with the following parameters:
        // encryptedData, authTag = ChaCha20-Poly1305(SessionKey, Nonce=”PS-Msg05”, AAD=<none>, Msg=<Sub-TLV>)
        let (mut ciphertext, mac) =
            chacha20_poly1305_encrypt(&session_key_2, b"\0\0\0\0PS-Msg05", &[], &sub_tlv)?;
        ciphertext.extend_from_slice(&mac);

        // Send the request to the accessory with the following TLV items:
        //     kTLVType_State <M5>
        //     kTLVType_EncryptedData <encryptedData with authTag appended>
        let state_item = tlv8::Item::new(tlv8::Tag::State, vec![PairingState::M5 as u8]);
        let encrypted_data_item = tlv8::Item::new(tlv8::Tag::EncryptedData, ciphertext);
        let encoded_tlv = tlv8::encode(&[state_item, encrypted_data_item], false);
        let encoded_tlv_len_str = encoded_tlv.len().to_string();

        let headers = [
            ("Content-Type", "application/octet-stream"),
            ("Content-Length", &encoded_tlv_len_str),
        ];

        debug!("pair-setup [5/5]: sending request...");

        let (resp_status, resp_body) = self
            .post_rtsp_with_resp("pair-setup", &encoded_tlv, Some(&headers))
            .await?;

        if resp_status == rtsp::StatusCode::Ok {
            let Some(body) = resp_body else {
                bail!("M4 -> M5 failed: missing body");
            };
            Ok(body)
        } else {
            Err(anyhow!("Failed to process M4 -> M5"))
        }
    }

    async fn pair_verify_m1(
        &mut self,
        fields: HashMap<tlv8::Tag, Vec<u8>>,
    ) -> anyhow::Result<Vec<u8>> {
        self.sender_state = SenderState::WaitingOnPairVerify1;

        let Some(encrypted_field) = fields.get(&tlv8::Tag::EncryptedData) else {
            bail!("Encrypted data missing");
        };
        ensure!(encrypted_field.len() >= TAG_LENGTH);
        let encrypted_tlv_data = &encrypted_field[..encrypted_field.len() - TAG_LENGTH];
        let tag_data = &encrypted_field[encrypted_field.len() - TAG_LENGTH..];

        let Some(k) = self.session_key.as_ref() else {
            bail!("No valid session key");
        };

        let mut session_key_2 = [0u8; 32];
        hkdf_extract_expand(
            k,
            b"Pair-Setup-Encrypt-Salt",
            b"Pair-Setup-Encrypt-Info",
            &mut session_key_2,
        )?;

        let decrypted_tlv = chacha20_poly1305_decrypt(
            &session_key_2,
            b"\0\0\0\0PS-Msg06",
            &[],
            encrypted_tlv_data,
            tag_data,
        )?;

        let accessory_items = tlv8::mapify(tlv8::decode(&decrypted_tlv)?);
        let Some(accessory_id_bytes) = accessory_items.get(&tlv8::Tag::Identifier) else {
            bail!("Missing accessory ID");
        };
        let Some(accessory_ltpk_bytes) = accessory_items.get(&tlv8::Tag::PublicKey) else {
            bail!("Missing accessory LTPK");
        };
        let Some(accessory_sig_bytes) = accessory_items.get(&tlv8::Tag::Signature) else {
            bail!("Missing accessory signature");
        };

        self.accessory_ltpk = Some(accessory_ltpk_bytes.to_vec());
        let mut accessory_x = [0u8; 32];
        hkdf_extract_expand(
            k,
            b"Pair-Setup-Accessory-Sign-Salt",
            b"Pair-Setup-Accessory-Sign-Info",
            &mut accessory_x,
        )?;

        let mut accessory_info = accessory_x.to_vec();
        accessory_info.extend_from_slice(accessory_id_bytes);
        accessory_info.extend_from_slice(accessory_ltpk_bytes);

        ensure!(accessory_ltpk_bytes.len() == 32);
        let mut accessory_ltpk_byte_slice = [0u8; 32];
        accessory_ltpk_byte_slice.copy_from_slice(&accessory_ltpk_bytes[0..32]);

        let verifier = ed25519_dalek::VerifyingKey::from_bytes(&accessory_ltpk_byte_slice)?;
        verifier
            .verify(
                &accessory_info,
                &ed25519_dalek::Signature::from_slice(accessory_sig_bytes)?,
            )
            .context("Accessory signature not verified")?;

        debug!("Accessory signature is valid");

        let curve_priv = x25519_dalek::EphemeralSecret::random();
        let curve_pub = x25519_dalek::PublicKey::from(&curve_priv);
        self.verifier_private_key = Some(curve_priv);
        self.verifier_public_key = Some(curve_pub);

        let state_item = tlv8::Item::new(tlv8::Tag::State, vec![PairingState::M1 as u8]);
        let pk_item = tlv8::Item::new(tlv8::Tag::PublicKey, curve_pub.as_bytes().to_vec());
        let encoded_tlv = tlv8::encode(&[state_item, pk_item], false);
        let encoded_tlv_len_str = encoded_tlv.len().to_string();

        let headers = [
            ("Content-Type", "application/octet-stream"),
            ("Content-Length", &encoded_tlv_len_str),
        ];

        debug!("pair-verify [1/2]: sending request...");

        let (resp_status, resp_body) = self
            .post_rtsp_with_resp("pair-verify", &encoded_tlv, Some(&headers))
            .await?;

        if resp_status == rtsp::StatusCode::Ok {
            let Some(body) = resp_body else {
                bail!("Failed to process pair-verify M1");
            };
            Ok(body)
        } else {
            Err(anyhow!("Pair-verify M1 failed"))
        }
    }

    // 5.7.3 M3: iOS Device -> Accessory - 'Verify Finish Request'
    async fn pair_verify_m2(
        &mut self,
        fields: HashMap<tlv8::Tag, Vec<u8>>,
    ) -> anyhow::Result<Vec<u8>> {
        self.sender_state = SenderState::WaitingOnPairVerify2;

        let Some(accessory_curve_pub_bytes) = fields.get(&tlv8::Tag::PublicKey) else {
            bail!("Public key missing");
        };
        let Some(accessory_encrypted_field) = fields.get(&tlv8::Tag::EncryptedData) else {
            bail!("Encrypted data missing");
        };
        self.accessory_curve_public = Some(accessory_curve_pub_bytes.clone());

        ensure!(accessory_encrypted_field.len() >= TAG_LENGTH);
        let encrypted_tlv_data =
            &accessory_encrypted_field[..accessory_encrypted_field.len() - TAG_LENGTH];
        let auth_tag = &accessory_encrypted_field[accessory_encrypted_field.len() - TAG_LENGTH..];

        let Some(priv_param) = self.verifier_private_key.take() else {
            bail!("Missing verifier");
        };

        ensure!(accessory_curve_pub_bytes.len() == 32);
        let mut accessory_curve_pub_slice = [0u8; 32];
        accessory_curve_pub_slice.copy_from_slice(&accessory_curve_pub_bytes[0..32]);

        // Generate the shared secret, SharedSecret, from its Curve25519 secret key and the accessoryʼs
        // Curve25519 public key.
        let pub_param = x25519_dalek::PublicKey::from(accessory_curve_pub_slice);
        let shared_secret = priv_param.diffie_hellman(&pub_param);

        //  Derive the symmetric session encryption key, SessionKey, in the same manner as the accessory.
        let mut session_key = [0u8; 32];
        hkdf_extract_expand(
            shared_secret.as_bytes(),
            b"Pair-Verify-Encrypt-Salt",
            b"Pair-Verify-Encrypt-Info",
            &mut session_key,
        )?;

        self.accessory_shared_secret = Some(shared_secret);

        // Verify the 16-byte auth tag, authTag, against the received encryptedData. If this fails, the
        // setup process will be aborted and an error will be reported to the user.
        // Decrypt the sub-TLV from the received encryptedData.
        let decrypted_tlv = chacha20_poly1305_decrypt(
            &session_key,
            b"\0\0\0\0PV-Msg02",
            &[],
            encrypted_tlv_data,
            auth_tag,
        )?;
        let accessory_items = tlv8::mapify(tlv8::decode(&decrypted_tlv)?);
        let accessory_id_bytes = accessory_items
            .get(&tlv8::Tag::Identifier)
            .ok_or(anyhow!("Missing accessory ID"))?;
        let accessory_sig_bytes = accessory_items
            .get(&tlv8::Tag::Signature)
            .ok_or(anyhow!("Missing accessory signature"))?;
        let verifier_public_key = self
            .verifier_public_key
            .as_ref()
            .ok_or(anyhow!("Missing verifier public key"))?;

        // Use the accessoryʼs Pairing Identifier to look up the accessoryʼs long-term public key, AccessoryLTPK,
        // in its list of paired accessories. If not found, the setup process will be aborted and an error will
        // be reported to the user.
        let Some(accessory_ltpk) = self.accessory_ltpk.as_ref() else {
            bail!("Missing accessory LTPK");
        };
        ensure!(accessory_ltpk.len() == 32);
        let mut accessory_ltpk_array = [0u8; 32];
        accessory_ltpk_array.copy_from_slice(&accessory_ltpk[0..32]);

        let accessory_info = [
            accessory_curve_pub_bytes.as_slice(),
            accessory_id_bytes.as_slice(),
            verifier_public_key.as_bytes(),
        ]
        .concat();
        let verifier = ed25519_dalek::VerifyingKey::from_bytes(&accessory_ltpk_array)?;
        verifier.verify(
            &accessory_info,
            &ed25519_dalek::Signature::from_slice(accessory_sig_bytes)?,
        )?;
        debug!("Accessory signature is valid");

        // Construct iOSDeviceInfo by concatenating the following items in order:
        //     (a) iOS Deviceʼs Curve25519 public key.
        //     (b) iOS Deviceʼs Pairing Identifier, iOSDevicePairingID.
        //     (c) Accessoryʼs Curve25519 public key from the received <M2> TLV
        let device_info = [
            verifier_public_key.as_bytes(),
            DEVICE_ID.as_bytes(),
            &accessory_curve_pub_slice,
        ]
        .concat();
        // Use Ed25519 to generate iOSDeviceSignature by signing iOSDeviceInfo with its long-term
        // secret key, iOSDeviceLTSK.
        let device_private_key = self
            .device_private_key
            .as_ref()
            .ok_or(anyhow!("Missing device private key"))?;
        let signature =
            ed25519_dalek::SigningKey::from_bytes(device_private_key).sign(&device_info);

        // Construct a sub-TLV with the following items:
        //     kTLVType_Identifier <iOSDevicePairingID>
        //     kTLVType_Signature <iOSDeviceSignature>
        let identifier_item = tlv8::Item::new(tlv8::Tag::Identifier, DEVICE_ID.as_bytes().to_vec());
        let signature_item = tlv8::Item::new(tlv8::Tag::Signature, signature.to_vec());
        let sub_tlv = tlv8::encode(&[identifier_item, signature_item], false);

        // Encrypt the sub-TLV, encryptedData, and generate the 16-byte auth tag, authTag. This uses the
        // ChaCha20-Poly1305 AEAD algorithm with the following parameters:
        // encryptedData, authTag = ChaCha20-Poly1305(SessionKey, Nonce=”PV-Msg03”, AAD=<none>, Msg=<Sub-TLV>)
        let (encrypted_data, auth_tag) =
            chacha20_poly1305_encrypt(&session_key, b"\0\0\0\0PV-Msg03", &[], &sub_tlv)?;

        // Construct the request with the following TLV items:
        //     kTLVType_State <M3>
        //     kTLVType_EncryptedData <encryptedData with authTag appended>
        let state_item = tlv8::Item::new(tlv8::Tag::State, vec![PairingState::M3 as u8]);
        let encrypted_data_item = tlv8::Item::new(
            tlv8::Tag::EncryptedData,
            [encrypted_data, auth_tag].concat(),
        );
        let encoded_response = tlv8::encode(&[state_item, encrypted_data_item], false);
        let encoded_response_len_str = encoded_response.len().to_string();

        let headers = [
            ("Content-Type", "application/octet-stream"),
            ("Content-Length", &encoded_response_len_str),
        ];

        debug!("pair-verify [2/2]: sending request...");

        // Send the request to the accessory.
        let (resp_status, resp_body) = self
            .post_rtsp_with_resp("pair-verify", &encoded_response, Some(&headers))
            .await?;

        if resp_status == rtsp::StatusCode::Ok {
            let Some(body) = resp_body else {
                bail!("Failed to process pair-verify M2");
            };
            Ok(body)
        } else {
            Err(anyhow!("Pair-verify M2 failed"))
        }
    }

    fn set_ciphers(&mut self) -> anyhow::Result<()> {
        // From https://openairplay.github.io/airplay-spec/pairing/hkp.html:
        // After successful pairing the connection switches to being encrypted using the format
        // N:n_bytes:tag where N is a 16 bit Little Endian length that describes the number of bytes
        // in n_bytes and n_bytes is encrypted using ChaCha20-Poly1305 with tag being the Poly1305 tag.
        //
        // Each direction uses its own key and nonce.
        //
        // The key for data sent from client to accessory is a HKDF-SHA-512 with the following parameters:
        //     InputKey = <EncryptionKey>
        //     Salt = ”Control-Salt”
        //     Info = ”Control-Write-Encryption-Key”
        //     OutputSize = 32 bytes
        //
        // While the data sent from accessory to client is HKDF-SHA-512 with the following parameters:
        //     InputKey = <EncryptionKey>
        //     Salt = ”Control-Salt”
        //     Info = ”Control-Read-Encryption-Key”
        //     OutputSize = 32 bytes
        //
        // The nonce is a 64 bit counter (i.e. the high order bits of the full 96 bit nonce is set to 0)
        // starting with 0 and incrementing by 1 for each encrypted block.
        let Some(shared_key) = self.accessory_shared_secret.as_ref() else {
            bail!("Missing accessory shared secret");
        };
        debug!("Shared key: {:?}", shared_key.as_bytes());
        hkdf_extract_expand(
            shared_key.as_bytes(),
            b"Control-Salt",
            b"Control-Write-Encryption-Key",
            &mut self.outgoing_key,
        )?;

        hkdf_extract_expand(
            shared_key.as_bytes(),
            b"Control-Salt",
            b"Control-Read-Encryption-Key",
            &mut self.incoming_key,
        )?;
        Ok(())
    }

    fn encrypt_data(&mut self, data: &[u8]) -> anyhow::Result<Vec<u8>> {
        if !self.is_encrypted {
            bail!("Cannot encrypt data because the connection is not encrypted");
        }

        let mut result = Vec::new();
        let mut offset = 0;
        while offset < data.len() {
            let length = MAX_BLOCK_LENGTH.min(data.len() - offset);
            let block_data = &data[offset..offset + length];
            let length_data = (length as u16).to_le_bytes();
            let nonce: [u8; 12] = {
                let b = self.out_count.to_le_bytes();
                [0, 0, 0, 0, b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]
            };
            let (ciphertext, mac) =
                chacha20_poly1305_encrypt(&self.outgoing_key, &nonce, &length_data, block_data)?;
            result.extend_from_slice(&length_data);
            result.extend_from_slice(&ciphertext);
            result.extend_from_slice(&mac);
            offset += length;
            self.out_count += 1;
        }

        Ok(result)
    }

    /// Try to decrypt the data in `data`.
    ///
    /// The format of `data` should be N:n_bytes:tag where N is a 16 bit Little Endian length that
    /// describes the number of bytes in n_bytes and n_bytes is encrypted using ChaCha20-Poly1305
    /// with tag being the Poly1305 tag.
    ///
    /// If `data` is incomplete, [`None`] is returned and more data should be read from the source.
    ///
    // TODO: pop res.len() + BLOCK_LENGTH_LENGTH + TAG_LENGTH from `data`
    fn decrypt_data(&mut self, data: &[u8]) -> anyhow::Result<Option<Vec<u8>>> {
        if !self.is_encrypted {
            bail!("Cannot decrypt data because the connection is not encrypted");
        }

        if data.len() < BLOCK_LENGTH_LENGTH {
            return Ok(None);
        }

        let length = u16::from_le_bytes([data[0], data[1]]) as usize;

        if data.len() < BLOCK_LENGTH_LENGTH + length + TAG_LENGTH {
            return Ok(None);
        }

        let ciphertext = &data[BLOCK_LENGTH_LENGTH..BLOCK_LENGTH_LENGTH + length];
        let auth_tag =
            &data[BLOCK_LENGTH_LENGTH + length..BLOCK_LENGTH_LENGTH + length + TAG_LENGTH];

        let nonce = {
            let b = self.in_count.to_le_bytes();
            [0, 0, 0, 0, b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]
        };

        let plaintext =
            chacha20_poly1305_decrypt(&self.incoming_key, &nonce, &[], ciphertext, auth_tag)?;

        Ok(Some(plaintext))
    }

    // async fn pairing_did_finish(&mut self) -> anyhow::Result<()> {
    //     debug!("Pairing succeeded. Device is ready.");
    //     let mut payload = json::Map::<String, json::Value>::new();
    //     payload.insert(
    //         "sessionUUID".to_owned(),
    //         json::json!(Uuid::new_v4().to_string()),
    //     );
    //     payload.insert("timingProtocol".to_owned(), json::json!("None"));
    //     let body = json::to_string(&payload).context("Failed to encode payload as json")?;

    //     let setup_request = format!(
    //         "SETUP /2182745467221657149 RTSP/1.0
    //         Content-Length: {}
    //         Content-Type: application/x-apple-binary-plist
    //         User-Agent: AirPlay/381.13
    //         X-Apple-HKP: 3
    //         X-Apple-StreamID: 1

    //         {body}",
    //         body.len()
    //     );
    //     let encrypted_data = self.encrypt_data(setup_request.as_bytes())?;
    //     self.post_http("2182745467221657149", &encrypted_data, None).await?;
    //     Ok(())
    // }

    // TODO: use http for HAP pairing
    async fn perform_pair(&mut self, pin: Option<&str>) -> anyhow::Result<()> {
        debug!("Starting pairing procedure...");

        let mut response_data = self.pair_setup_m1_m2(pin).await?;

        loop {
            debug!("Current pair state: {:?}", self.sender_state);

            let item_map = tlv8::mapify(tlv8::decode(&response_data)?);
            // debug!("Item map: {item_map:?}");

            if let Some(error_bytes) = item_map.get(&tlv8::Tag::Error) {
                if !error_bytes.is_empty() {
                    let error_code = error_bytes[0];
                    if error_code == 0x03 {
                        let backoff_bytes = item_map
                            .get(&tlv8::Tag::RetryDelay)
                            .ok_or(anyhow!("Missing `RetryDelay` item"))?;
                        if backoff_bytes.len() == 2 {
                            let backoff_seconds =
                                i16::from_le_bytes([backoff_bytes[0], backoff_bytes[1]]);
                            bail!(
                                "Pairing backoff requested, should retry in {backoff_seconds} seconds"
                            );
                        } else {
                            todo!("Backoff bytes: {}", backoff_bytes.len());
                        }
                    } else {
                        bail!("Pairing failed with error code {error_code}");
                    }
                }
            }

            let Some(state_bytes) = item_map.get(&tlv8::Tag::State) else {
                bail!("State item missing");
            };
            if state_bytes.is_empty() {
                bail!("State item missing");
            }

            let remote_state = PairingState::try_from(state_bytes[0])?;
            debug!("Transitioned to state {remote_state:?}");

            match self.sender_state {
                SenderState::WaitingOnPairSetup1 if remote_state == PairingState::M2 => {
                    response_data = self.pair_setup_m2_m3(item_map).await?;
                }
                SenderState::WaitingOnPairSetup2 if remote_state == PairingState::M4 => {
                    response_data = self.pair_setup_m4_m5(item_map).await?;
                }
                SenderState::WaitingOnPairSetup3 if remote_state == PairingState::M6 => {
                    response_data = self.pair_verify_m1(item_map).await?;
                }
                SenderState::WaitingOnPairVerify1 if remote_state == PairingState::M2 => {
                    response_data = self.pair_verify_m2(item_map).await?;
                }
                SenderState::WaitingOnPairVerify2 if remote_state == PairingState::M4 => {
                    self.sender_state = SenderState::ReadyToPlay;
                    self.set_ciphers()?;
                    self.is_encrypted = true;
                    self.paired = true;
                    // self.pairing_did_finish().await?;
                    break;
                }
                _ => bail!(
                    "Unexpected state `{remote_state:?}` when in {:?}",
                    self.sender_state
                ),
            }
        }

        debug!("Pairing completed");

        Ok(())
    }

    async fn fetch_info(&mut self) -> anyhow::Result<airplay_common::InfoPlist> {
        let cseq_str = self.rtsp_cseq.to_string();
        self.rtsp_cseq += 1;
        let req = rtsp::Request {
            method: rtsp::Method::Get,
            path: "/info",
            version: rtsp::Version::Rtsp10,
            headers: &[
                ("X-Apple-ProtocolVersion", "1"),
                ("CSeq", &cseq_str),
                ("User-Agent", "AirPlay/381.13"),
            ],
            body: None,
        };

        let (resp_status, resp_body) = self.send_rtsp_request(req).await?;
        if resp_status != rtsp::StatusCode::Ok {
            bail!("Failed to get `/info`, status code: {resp_status:?}");
        }

        let Some(body) = resp_body else {
            bail!("Failed to get `/info`: missing body");
        };

        Ok(info_from_plist(&body)?)
    }

    async fn inner_work(
        &mut self,
        addrs: Vec<SocketAddr>,
        mut cmd_rx: Receiver<Command>,
    ) -> anyhow::Result<()> {
        self.event_handler
            .connection_state_changed(DeviceConnectionState::Connecting);

        let Some(stream) =
            utils::try_connect_tcp(addrs, 5, &mut cmd_rx, |cmd| cmd == Command::Quit).await?
        else {
            debug!("Received Quit command in connect loop");
            self.event_handler
                .connection_state_changed(DeviceConnectionState::Disconnected);
            return Ok(());
        };

        self.used_remote_addr = Some(stream.peer_addr()?);

        self.event_handler
            .connection_state_changed(DeviceConnectionState::Connected {
                used_remote_addr: stream
                    .peer_addr()
                    .context("Failed to get peer address")?
                    .ip()
                    .into(),
                local_addr: stream
                    .local_addr()
                    .context("Failed to get local address")?
                    .ip()
                    .into(),
            });

        self.stream = Some(stream);

        let info = self.fetch_info().await?;
        if let Some(features) = info.features.as_ref() {
            // TODO: what are the different pairing protocols?
            if features.contains(AirPlayFeatures::SupportsHKPairingAndAccessControl) {
            } else if features.contains(AirPlayFeatures::Authentication4) {
            }
            debug!("{features:?}");
        };

        debug!("status: {:?}", info.status_flags);
        debug!("info: {info:?}");

        self.perform_pair(None).await?;

        let mut feedback_interval = tokio::time::interval(FEEDBACK_INTERVAL);

        loop {
            tokio::select! {
                cmd = cmd_rx.recv() => {
                    let cmd = cmd.ok_or(anyhow!("No more commands"))?;
                    debug!("Received command: {cmd:?}");
                    match cmd {
                        Command::Quit => break,
                    }
                }
                _ = feedback_interval.tick() => self.send_feedback().await?,
            }
        }

        Ok(())
    }

    pub async fn work(mut self, addrs: Vec<SocketAddr>, cmd_rx: Receiver<Command>) {
        debug!("Starting to work...");

        if let Err(err) = self.inner_work(addrs, cmd_rx).await {
            error!("Inner work error: {err}");
        }

        self.event_handler
            .connection_state_changed(DeviceConnectionState::Disconnected);
    }
}

struct State {
    rt_handle: Handle,
    started: bool,
    command_tx: Option<Sender<Command>>,
    addresses: Vec<IpAddr>,
    name: String,
    port: u16,
}

impl State {
    pub fn new(device_info: DeviceInfo, rt_handle: Handle) -> Self {
        Self {
            rt_handle,
            started: false,
            command_tx: None,
            addresses: device_info.addresses,
            name: device_info.name,
            port: device_info.port,
        }
    }
}

#[cfg_attr(feature = "uniffi", derive(uniffi::Object))]
pub struct AirPlay2Device {
    state: Mutex<State>,
}

impl AirPlay2Device {
    const SUPPORTED_FEATURES: [DeviceFeature; 0] = [];

    pub fn new(device_info: DeviceInfo, rt_handle: Handle) -> Self {
        Self {
            state: Mutex::new(State::new(device_info, rt_handle)),
        }
    }
}

impl AirPlay2Device {
    fn send_command(&self, cmd: Command) -> Result<(), CastingDeviceError> {
        let state = self.state.lock().unwrap();
        let Some(tx) = &state.command_tx else {
            error!("Missing command tx");
            return Err(CastingDeviceError::FailedToSendCommand);
        };

        debug!("Sending command: {cmd:?}");
        // TODO: `blocking_send()`? Would need to check for a runtime and use that if it exists.
        //        Can save clones when this function is called from sync environment.
        let tx = tx.clone();
        // state.runtime.spawn(async move { tx.send(cmd).await });
        state.rt_handle.spawn(async move { tx.send(cmd).await });

        Ok(())
    }
}

impl CastingDevice for AirPlay2Device {
    fn casting_protocol(&self) -> ProtocolType {
        ProtocolType::AirPlay2
    }

    fn is_ready(&self) -> bool {
        let state = self.state.lock().unwrap();
        !state.addresses.is_empty() && state.port > 0 && !state.name.is_empty()
    }

    fn supports_feature(&self, feature: DeviceFeature) -> bool {
        Self::SUPPORTED_FEATURES.contains(&feature)
    }

    fn name(&self) -> String {
        let state = self.state.lock().unwrap();
        state.name.clone()
    }

    fn set_name(&self, name: String) {
        let mut state = self.state.lock().unwrap();
        state.name = name;
    }

    fn stop_casting(&self) -> Result<(), CastingDeviceError> {
        if let Err(err) = self.stop_playback() {
            error!("Failed to stop playback: {err}");
        }
        info!("Stopping active device because stopCasting was called.");
        self.disconnect()
    }

    fn seek(&self, _time_seconds: f64) -> Result<(), CastingDeviceError> {
        todo!()
    }

    fn stop_playback(&self) -> Result<(), CastingDeviceError> {
        todo!()
    }

    fn pause_playback(&self) -> Result<(), CastingDeviceError> {
        todo!()
    }

    fn resume_playback(&self) -> Result<(), CastingDeviceError> {
        todo!()
    }

    fn load_url(
        &self,
        _content_type: String,
        _url: String,
        _resume_position: Option<f64>,
        _speed: Option<f64>,
    ) -> Result<(), CastingDeviceError> {
        todo!()
    }

    fn load_content(
        &self,
        _content_type: String,
        _content: String,
        _resume_position: f64,
        _duration: f64,
        _speed: Option<f64>,
    ) -> Result<(), CastingDeviceError> {
        todo!()
    }

    // https://github.com/postlund/pyatv/blob/49f9c9e960930276c8fac9fb7696b54a7beb1951/pyatv/protocols/raop/protocols/airplayv2.py#L210
    fn load_video(
        &self,
        _content_type: String,
        _url: String,
        _resume_position: f64,
        _speed: Option<f64>,
    ) -> Result<(), CastingDeviceError> {
        todo!()
    }

    fn load_image(&self, _content_type: String, _url: String) -> Result<(), CastingDeviceError> {
        todo!()
    }

    fn change_volume(&self, _volume: f64) -> Result<(), CastingDeviceError> {
        todo!()
    }

    fn change_speed(&self, _speed: f64) -> Result<(), CastingDeviceError> {
        todo!()
    }

    fn disconnect(&self) -> Result<(), CastingDeviceError> {
        if let Err(err) = self.send_command(Command::Quit) {
            error!("Failed to stop worker: {err}");
        }
        let mut state = self.state.lock().unwrap();
        state.command_tx = None;
        state.started = false;
        Ok(())
    }

    fn connect(
        &self,
        event_handler: Arc<dyn DeviceEventHandler>,
    ) -> Result<(), CastingDeviceError> {
        let mut state = self.state.lock().unwrap();
        if state.started {
            return Err(CastingDeviceError::DeviceAlreadyStarted);
        }

        let addrs = crate::casting_device::ips_to_socket_addrs(&state.addresses, state.port);
        if addrs.is_empty() {
            return Err(CastingDeviceError::MissingAddresses);
        }

        state.started = true;
        info!("Starting with address list: {addrs:?}...");

        let (tx, rx) = tokio::sync::mpsc::channel::<Command>(50);
        state.command_tx = Some(tx);

        state
            .rt_handle
            .spawn(InnerDevice::new(event_handler).work(addrs, rx));

        Ok(())
    }

    fn get_device_info(&self) -> DeviceInfo {
        todo!()
    }

    fn get_addresses(&self) -> Vec<IpAddr> {
        let state = self.state.lock().unwrap();
        state.addresses.clone()
    }

    fn set_addresses(&self, addrs: Vec<IpAddr>) {
        let mut state = self.state.lock().unwrap();
        state.addresses = addrs;
    }

    fn get_port(&self) -> u16 {
        let state = self.state.lock().unwrap();
        state.port
    }

    fn set_port(&self, port: u16) {
        let mut state = self.state.lock().unwrap();
        state.port = port;
    }

    fn subscribe_event(
        &self,
        _group: GenericEventSubscriptionGroup,
    ) -> Result<(), CastingDeviceError> {
        Err(CastingDeviceError::UnsupportedSubscription)
    }

    fn unsubscribe_event(
        &self,
        _group: GenericEventSubscriptionGroup,
    ) -> Result<(), CastingDeviceError> {
        Err(CastingDeviceError::UnsupportedSubscription)
    }
}

#[cfg(test)]
mod tests {
    use super::{srp_client::SrpClient, *};

    #[test]
    fn test_srp() {
        // Username
        let i = "alice";
        // Password
        let p = "password123";
        // A private
        let a = BigUint::from_bytes_be(&hex!(
            "60975527 035CF2AD 1989806F 0407210B C81EDC04 E2762A56 AFD529DD DA2D4393"
        ));
        // A public
        let big_a = BigUint::from_bytes_be(&hex!(
            "FAB6F5D2 615D1E32 3512E799 1CC37443 F487DA60 4CA8C923 0FCB04E5 41DCE628
            0B27CA46 80B0374F 179DC3BD C7553FE6 2459798C 701AD864 A91390A2 8C93B644
            ADBF9C00 745B942B 79F9012A 21B9B787 82319D83 A1F83628 66FBD6F4 6BFC0DDB
            2E1AB6E4 B45A9906 B82E37F0 5D6F97F6 A3EB6E18 2079759C 4F684783 7B62321A
            C1B4FA68 641FCB4B B98DD697 A0C73641 385F4BAB 25B79358 4CC39FC8 D48D4BD8
            67A9A3C1 0F8EA121 70268E34 FE3BBE6F F89998D6 0DA2F3E4 283CBEC1 393D52AF
            724A5723 0C604E9F BCE583D7 613E6BFF D67596AD 121A8707 EEC46944 95703368
            6A155F64 4D5C5863 B48F61BD BF19A53E AB6DAD0A 186B8C15 2E5F5D8C AD4B0EF8
            AA4EA500 8834C3CD 342E5E0F 167AD045 92CD8BD2 79639398 EF9E114D FAAAB919
            E14E8509 89224DDD 98576D79 385D2210 902E9F9B 1F2D86CF A47EE244 635465F7
            1058421A 0184BE51 DD10CC9D 079E6F16 04E7AA9B 7CF7883C 7D4CE12B 06EBE160
            81E23F27 A231D184 32D7D1BB 55C28AE2 1FFCF005 F57528D1 5A88881B B3BBB7FE"
        ));
        // B private
        // let b = &hex!("E487CB59 D31AC550 471E81F0 0F6928E0 1DDA08E9 74A004F4 9E61F5D1 05284D20");
        // B public
        let big_b = &hex!(
            "40F57088 A482D4C7 733384FE 0D301FDD CA9080AD 7D4F6FDF 09A01006 C3CB6D56 \
            2E41639A E8FA21DE 3B5DBA75 85B27558 9BDB2798 63C56280 7B2B9908 3CD1429C \
            DBE89E25 BFBD7E3C AD3173B2 E3C5A0B1 74DA6D53 91E6A06E 465F037A 40062548 \
            39A56BF7 6DA84B1C 94E0AE20 8576156F E5C140A4 BA4FFC9E 38C3B07B 88845FC6 \
            F7DDDA93 381FE0CA 6084C4CD 2D336E54 51C464CC B6EC65E7 D16E548A 273E8262 \
            84AF2559 B6264274 215960FF F47BDD63 D3AFF064 D6137AF7 69661C9D 4FEE4738 \
            2603C88E AA098058 1D077584 61B777E4 356DDA58 35198B51 FEEA308D 70F75450 \
            B71675C0 8C7D8302 FD7539DD 1FF2A11C B4258AA7 0D234436 AA42B6A0 615F3F91 \
            5D55CC3B 966B2716 B36E4D1A 06CE5E5D 2EA3BEE5 A1270E87 51DA45B6 0B997B0F \
            FDB0F996 2FEE4F03 BEE780BA 0A845B1D 92714217 83AE6601 A61EA2E3 42E4F2E8 \
            BC935A40 9EAD19F2 21BD1B74 E2964DD1 9FC845F6 0EFC0933 8B60B6B2 56D8CAC8 \
            89CCA306 CC370A0B 18C8B886 E95DA0AF 5235FEF4 393020D2 B7F30569 04759042"
        );
        // Salt
        let s = &hex!("BEB25379 D1A8581E B5A72767 3A2441EE");
        // Verifier
        let v = BigUint::from_bytes_be(&hex!(
            "9B5E0617 01EA7AEB 39CF6E35 19655A85 3CF94C75 CAF2555E F1FAF759 BB79CB47 \
            7014E04A 88D68FFC 05323891 D4C205B8 DE81C2F2 03D8FAD1 B24D2C10 9737F1BE \
            BBD71F91 2447C4A0 3C26B9FA D8EDB3E7 80778E30 2529ED1E E138CCFC 36D4BA31 \
            3CC48B14 EA8C22A0 186B222E 655F2DF5 603FD75D F76B3B08 FF895006 9ADD03A7 \
            54EE4AE8 8587CCE1 BFDE3679 4DBAE459 2B7B904F 442B041C B17AEBAD 1E3AEBE3 \
            CBE99DE6 5F4BB1FA 00B0E7AF 06863DB5 3B02254E C66E781E 3B62A821 2C86BEB0 \
            D50B5BA6 D0B478D8 C4E9BBCE C2176532 6FBD1405 8D2BBDE2 C33045F0 3873E539 \
            48D78B79 4F0790E4 8C36AED6 E880F557 427B2FC0 6DB5E1E2 E1D7E661 AC482D18 \
            E528D729 5EF74372 95FF1A72 D4027717 13F16876 DD050AE5 B7AD53CC B90855C9 \
            39566483 58ADFD96 6422F524 98732D68 D1D7FBEF 10D78034 AB8DCB6F 0FCF885C \
            C2B2EA2C 3E6AC866 09EA058A 9DA8CC63 531DC915 414DF568 B09482DD AC1954DE \
            C7EB714F 6FF7D44C D5B86F6B D1158109 30637C01 D0F6013B C9740FA2 C633BA89"
        ));
        // Random scrambling parameter
        let u = BigUint::from_bytes_be(&hex!(
            "03AE5F3C 3FA9EFF1 A50D7DBB 8D2F60A1 EA66EA71 2D50AE97 6EE34641 A1CD0E51 \
             C4683DA3 83E8595D 6CB56A15 D5FBC754 3E07FBDD D316217E 01A391A1 8EF06DFF"
        ));
        // Premaster secret
        let big_s = BigUint::from_bytes_be(&hex!(
            "F1036FEC D017C823 9C0D5AF7 E0FCF0D4 08B009E3 6411618A 60B23AAB BFC38339 \
            72682312 14BAACDC 94CA1C53 F442FB51 C1B027C3 18AE238E 16414D60 D1881B66 \
            486ADE10 ED02BA33 D098F6CE 9BCF1BB0 C46CA2C4 7F2F174C 59A9C61E 2560899B \
            83EF6113 1E6FB30B 714F4E43 B735C9FE 6080477C 1B83E409 3E4D456B 9BCA492C \
            F9339D45 BC42E67C E6C02C24 3E49F5DA 42A869EC 855780E8 4207B8A1 EA6501C4 \
            78AAC0DF D3D22614 F531A00D 826B7954 AE8B14A9 85A42931 5E6DD366 4CF47181 \
            496A9432 9CDE8005 CAE63C2F 9CA4969B FE840019 24037C44 6559BDBB 9DB9D4DD \
            142FBCD7 5EEF2E16 2C843065 D99E8F05 762C4DB7 ABD9DB20 3D41AC85 A58C05BD \
            4E2DBF82 2A934523 D54E0653 D376CE8B 56DCB452 7DDDC1B9 94DC7509 463A7468 \
            D7F02B1B EB168571 4CE1DD1E 71808A13 7F788847 B7C6B7BF A1364474 B3B7E894 \
            78954F6A 8E68D45B 85A88E4E BFEC1336 8EC0891C 3BC86CF5 00978801 78D86135 \
            E7287234 58538858 D715B7B2 47406222 C1019F53 603F0169 52D49710 0858824C"
        ));
        // Session key
        let big_k = &hex!(
            "5CBC219D B052138E E1148C71 CD449896 3D682549 CE91CA24 F098468F 06015BEB \
            6AF245C2 093F98C3 651BCA83 AB8CAB2B 580BBF02 184FEFDF 26142F73 DF95AC50"
        );

        let mut srp = SrpClient::new(
            srp_group_n(),
            srp_group_g(),
            i.as_bytes().to_vec(),
            p.as_bytes().to_vec(),
        );
        let big_a_computed = srp.srp_user_start_authentication(Some(a)).unwrap();
        assert_eq!(big_a, big_a_computed);

        let triple = srp.user_process_challenge_internal(s, big_b).unwrap();
        let u_computed = triple.0;
        let v_computed = triple.1;
        // let big_m_computed = triple.2;
        assert_eq!(u_computed, u);
        assert_eq!(v_computed, v);
        let big_s_computed = srp.get_big_s().unwrap();
        assert_eq!(big_s_computed, big_s);

        assert_eq!(srp.get_session_key().unwrap(), big_k);
    }
}
