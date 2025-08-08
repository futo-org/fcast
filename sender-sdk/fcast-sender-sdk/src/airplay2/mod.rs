mod srp;
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
use plist::Value;
// use plist::PlistParseError;
use rand_pcg::rand_core::RngCore;
use rtsp::StatusCode;
use sha2::Sha512;
use srp::SrpClient;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    runtime::Handle,
    sync::mpsc::{Receiver, Sender},
    time::timeout,
};
use utils::hexdump;
use uuid::Uuid;

use crate::{
    airplay_common::{self, AirPlayFeatures, AirPlayStatus, InfoPlist},
    casting_device::{
        CastingDevice, CastingDeviceError, DeviceConnectionState, DeviceEventHandler,
        DeviceFeature, DeviceInfo, GenericEventSubscriptionGroup, ProtocolType,
    },
    net_utils, IpAddr,
};

use parsers_common::{find_first_cr_lf, find_first_double_cr_lf, parse_header_map};

const DEVICE_ID: &str = "C9635ED0964902E0";
const PAIRING_USERNAME: &str = "Pair-Setup";
const TAG_LENGTH: usize = 16; // Poly1305
const MAX_BLOCK_LENGTH: usize = 0x400;
const BLOCK_LENGTH_LENGTH: usize = 2; // u16
const FEEDBACK_INTERVAL: Duration = Duration::from_secs(2); // https://github.com/postlund/pyatv/blob/49f9c9e960930276c8fac9fb7696b54a7beb1951/pyatv/protocols/raop/protocols/airplayv2.py#L25
const REQ_TIMEOUT_DUR: Duration = Duration::from_secs(5);
const BPLIST_CONTENT_TYPE: &str = "application/x-apple-binary-plist";

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

// pub fn info_from_plist(plist: &[u8]) -> Result<InfoPlist, PlistParseError> {
pub fn info_from_plist(data: &[u8]) -> anyhow::Result<InfoPlist> {
    let info: InfoPlist = plist::from_bytes(data)?;
    // let mut reader = plist::PlistReader::new(plist);
    // reader.read_magic_number()?;
    // reader.read_version()?;
    // let trailer = reader.read_trailer()?;
    // let mut info = InfoPlist::default();
    // let mut parser = plist::PlistParser::new(plist, trailer)?;
    // let parsed = parser.parse()?;
    // for obj in parsed {
    //     if let plist::Object::Dict(items) = obj {
    //         for (key, val) in items {
    //             let plist::Object::String(key) = key else {
    //                 continue;
    //             };
    //             match key.as_str() {
    //                 "features" => {
    //                     if let plist::Object::Int(features) = val {
    //                         info.features =
    //                             Some(AirPlayFeatures::from_bits_truncate(features as u64));
    //                     }
    //                 }
    //                 "statusFlags" => {
    //                     if let plist::Object::Int(status_flags) = val {
    //                         info.status_flags =
    //                             Some(AirPlayStatus::from_bits_truncate(status_flags as u32));
    //                     }
    //                 }
    //                 _ => (),
    //             }
    //         }
    //     }
    // }

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

    // ////////////////////////////////////////
    debug!("Encrypt:");
    debug!("key: {key:?}");
    debug!("nonce: {nonce:?}");
    debug!("aad: {aad:?}");
    // ////////////////////////////////////////

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
    // //////////////////////////////////////////////////
    // debug!("key: {key:?}");
    // debug!("nonce: {nonce:?}");
    // debug!("aad: {aad:?}");
    // debug!("ciphertext: {ciphertext:?}");
    // debug!("mac: {mac:?}");
    // //////////////////////////////////////////////////
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

bitflags::bitflags! {
    pub struct PairingFlags: u32 {
        /// Pair Setup M1 - M4 without exchanging public keys
        const Transient = 1 << 4;
    }
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
        // req: rtsp::Request<'_>,
        // mut req: rtsp::Request<'_>,
        req: rtsp::Request<'_>,
    ) -> anyhow::Result<(StatusCode, Option<Vec<u8>>)> {
        let Some(stream) = self.stream.as_mut() else {
            bail!("Cannot send request because stream is missing");
        };

        // let mut he = req.headers.to_vec();
        // he.push(("Authorization", "Basic UGFpci1TZXR1cDozOTM5"));
        // req.headers = &he;

        self.scratch_buffer.clear();
        req.encode_into(&mut self.scratch_buffer);
        debug!("{}", hexdump(&self.scratch_buffer));
        timeout(REQ_TIMEOUT_DUR, stream.write_all(&self.scratch_buffer)).await??;
        stream.flush().await?;

        // TODO: make sure we return the right response for the CSeq sent now

        self.scratch_buffer.clear();
        let read_buf = &mut self.scratch_buffer;
        let mut tmp_buf = [0u8; 1024];

        loop {
            let read_bytes = timeout(REQ_TIMEOUT_DUR, stream.read(&mut tmp_buf)).await??;
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
                    let read_bytes = timeout(REQ_TIMEOUT_DUR, stream.read(&mut tmp_buf)).await??;
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
                let read_bytes = timeout(REQ_TIMEOUT_DUR, stream.read(&mut tmp_buf)).await??;
                if read_bytes == 0 {
                    bail!(
                        "Failed to read body, {} bytes are missing",
                        content_length - read_buf.len() - headers_end
                    );
                }
                read_buf.extend_from_slice(&tmp_buf[0..read_bytes]);
            }
            debug!("--- Response ---\n{}", hexdump(read_buf));
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
            // ("X-Apple-HKP", "4"),
            // ("X-Apple-ProtocolVersion", "1"),
            // ("X-Apple-Client-Name", "FCast Sender SDK"),
            ("CSeq", &cseq_str),
            // ("Host", "192.168.1.203:7000"),
            // ("Connection", "Keep-Alive"),
            // ("Authorization", "Basic UGFpci1TZXR1cDozOTM5"),
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

        debug!("Sending request: {req:?}");

        // let mut encoded_req = Vec::new();
        // req.encode_into(&mut encoded_req);

        let res = self.post_rtsp_with_resp("feedback", &[], None).await?;

        debug!("{res:?}");

        // let encrypted = self.encrypt_data(&encoded_req)?;

        // match self.stream.as_mut() {
        //     Some(stream) => stream.write_all(&encrypted).await?,
        //     None => bail!("Cannot send feedback because stream is missing"),
        // }

        // let mut read_buf = Vec::new();
        // let mut tmp_buf = [0u8; 1024];
        // loop {
        //     match self.stream.as_mut() {
        //         Some(stream) => {
        //             let n_read = stream.read(&mut tmp_buf).await?;
        //             read_buf.extend_from_slice(&tmp_buf[0..n_read]);
        //             if n_read < tmp_buf.len() {
        //                 break;
        //             }
        //         }
        //         None => bail!("Cannot send feedback because stream is missing"),
        //     }
        // }

        // if let Some(plaintext) = self.decrypt_data(&read_buf)? {
        //     debug!("Response:\n{}", hexdump::hexdump(&plaintext));
        // } else {
        //     debug!("Could not decrypt");
        // }

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

        // When the iOS device performs Pair Setup with a separate optional authentication procedure, it sends a request to the
        // accessory with the following TLV items:
        //     kTLVType_State <M1>
        //     kTLVType_Method <Pair Setup>
        //     kTLVType_Flags <Pairing Type Flags>
        let state_item = tlv8::Item::new(tlv8::Tag::State, vec![PairingState::M1 as u8]);
        let method_item = tlv8::Item::new(tlv8::Tag::Method, vec![PairingMethod::PairSetup as u8]);
        let flags_item = tlv8::Item::new(
            tlv8::Tag::Flags,
            PairingFlags::Transient.bits().to_be_bytes().to_vec(),
        );
        let encoded_tlv = tlv8::encode(&[state_item, method_item, flags_item], false);
        let encoded_tlv_len_str = encoded_tlv.len().to_string();

        let headers = [
            ("Content-Type", "application/pairing+tlv8"),
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
        // self.sender_state = SenderState::WaitingOnPairVerify1;

        let Some(salt_bytes) = fields.get(&tlv8::Tag::Salt) else {
            bail!("Missing salt");
        };

        debug!("Salt (len={}):\n{}", salt_bytes.len(), hexdump(salt_bytes));

        let Some(b_bytes) = fields.get(&tlv8::Tag::PublicKey) else {
            bail!("Missing public key");
        };

        let Some(client) = self.srp_client.as_mut() else {
            bail!("SRP client is not initialized");
        };

        // 4. Generate its SRP public key with SRP_gen_pub().
        debug!("Starting authentication");
        let a_pub = client.srp_user_start_authentication(None)?;

        debug!(
            "Public key (len={}): {}",
            a_pub.to_bytes_be().len(),
            hexdump(&a_pub.to_bytes_be())
        );

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
            ("Content-Type", "application/pairing+tlv8"),
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
        // ) -> anyhow::Result<Vec<u8>> {
    ) -> anyhow::Result<()> {
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

        debug!("session key length {}", session_key.len());

        hkdf_extract_expand(
            &session_key,
            b"Control-Salt",
            b"Control-Write-Encryption-Key",
            &mut self.outgoing_key,
        )?;

        hkdf_extract_expand(
            &session_key,
            b"Control-Salt",
            b"Control-Read-Encryption-Key",
            &mut self.incoming_key,
        )?;

        // debug!("incoming key: {incoming_key:?}");

        // WE ARE NOT DOING NON-TRANSIENT PAIR-SETUP. STOP HERE.
        // TODO: carefully read the homekit pairing spec and implement acordingly

        // Generate its Ed25519 long-term public key, iOSDeviceLTPK, and long-term secret key, iOSDeviceLTSK.
        // let mut rng = rand_pcg::Pcg64::new(0xcafef00dd15ea5e5, 0xa02bdbf7bb3c0a7);
        // let mut seed = [0u8; 32];
        // rng.fill_bytes(&mut seed);
        // let ed_priv = ed25519_dalek::SigningKey::from_bytes(&seed);
        // let ed_pub = ed_priv.verifying_key().to_bytes();
        // // self.device_private_key = Some(ed_priv.to_keypair_bytes().to_bytes());
        // self.device_private_key = Some(ed_priv.to_bytes());
        // self.device_public_key = Some(ed_pub);

        // let mut device_x = [0u8; 32];
        // hkdf_extract_expand(
        //     &session_key,
        //     b"Pair-Setup-Controller-Sign-Salt",
        //     b"Pair-Setup-Controller-Sign-Info",
        //     &mut device_x,
        // )?;

        // // Concatenate iOSDeviceX with the iOS deviceʼs Pairing Identifier, iOSDevicePairingID, and its long-term
        // // public key, iOSDeviceLTPK. The data must be concatenated in order such that the final data is iOSDeviceX,
        // // iOSDevicePairingID, iOSDeviceLTPK. The concatenated value will be referred to as iOSDeviceInfo.
        // self.scratch_buffer.clear();
        // let device_info = &mut self.scratch_buffer;
        // device_info.extend_from_slice(&device_x);
        // device_info.extend_from_slice(DEVICE_ID.as_bytes());
        // device_info.extend_from_slice(&ed_pub);
        // // Generate iOSDeviceSignature by signing iOSDeviceInfo with its long-term secret key, iOSDeviceLTSK,
        // // using Ed25519.
        // let signature = ed_priv.sign(device_info).to_bytes();

        // // Construct a sub-TLV with the following TLV items:
        // //     kTLVType_Identifier <iOSDevicePairingID>
        // //     kTLVType_PublicKey <iOSDeviceLTPK>
        // //     kTLVType_Signature <iOSDeviceSignature>
        // let identifier_item = tlv8::Item::new(tlv8::Tag::Identifier, DEVICE_ID.as_bytes().to_vec());
        // let public_key_item = tlv8::Item::new(tlv8::Tag::PublicKey, ed_pub.to_vec());
        // let sig_item = tlv8::Item::new(tlv8::Tag::Signature, signature.to_vec());
        // let sub_tlv = tlv8::encode(&[identifier_item, public_key_item, sig_item], false);

        // let mut session_key_2 = [0u8; 32];
        // hkdf_extract_expand(
        //     &session_key,
        //     "Pair-Setup-Encrypt-Salt".as_bytes(),
        //     "Pair-Setup-Encrypt-Info".as_bytes(),
        //     &mut session_key_2,
        // )?;

        // // Encrypt the sub-TLV, encryptedData, and generate the 16 byte auth tag, authTag. This uses the
        // // ChaCha20-Poly1305 AEAD algorithm with the following parameters:
        // // encryptedData, authTag = ChaCha20-Poly1305(SessionKey, Nonce=”PS-Msg05”, AAD=<none>, Msg=<Sub-TLV>)
        // let (mut ciphertext, mac) =
        //     chacha20_poly1305_encrypt(&session_key_2, b"\0\0\0\0PS-Msg05", &[], &sub_tlv)?;
        // ciphertext.extend_from_slice(&mac);

        // // Send the request to the accessory with the following TLV items:
        // //     kTLVType_State <M5>
        // //     kTLVType_EncryptedData <encryptedData with authTag appended>
        // let state_item = tlv8::Item::new(tlv8::Tag::State, vec![PairingState::M5 as u8]);
        // let encrypted_data_item = tlv8::Item::new(tlv8::Tag::EncryptedData, ciphertext);
        // let encoded_tlv = tlv8::encode(&[state_item, encrypted_data_item], false);
        // let encoded_tlv_len_str = encoded_tlv.len().to_string();

        // let headers = [
        //     ("Content-Type", "application/pairing+tlv8"),
        //     ("Content-Length", &encoded_tlv_len_str),
        // ];

        // debug!("pair-setup [5/5]: sending request...");

        // let (resp_status, resp_body) = self
        //     .post_rtsp_with_resp("pair-setup", &encoded_tlv, Some(&headers))
        //     .await?;

        // if resp_status == rtsp::StatusCode::Ok {
        //     let Some(body) = resp_body else {
        //         bail!("M4 -> M5 failed: missing body");
        //     };
        //     Ok(body)
        // } else {
        //     Err(anyhow!("Failed to process M4 -> M5"))
        // }

        // Err(anyhow!("Failed to process M4 -> M5"))
        Ok(())
    }

    // async fn pair_verify_m1(
    //     &mut self,
    //     fields: HashMap<tlv8::Tag, Vec<u8>>,
    // ) -> anyhow::Result<Vec<u8>> {
    //     self.sender_state = SenderState::WaitingOnPairVerify1;

    //     let Some(encrypted_field) = fields.get(&tlv8::Tag::EncryptedData) else {
    //         bail!("Encrypted data missing");
    //     };
    //     ensure!(encrypted_field.len() >= TAG_LENGTH);
    //     let ed_tlv_data = &encrypted_field[..encrypted_field.len() - TAG_LENGTH];
    //     let tag_data = &encrypted_field[encrypted_field.len() - TAG_LENGTH..];

    //     let Some(k) = self.session_key.as_ref() else {
    //         bail!("No valid session key");
    //     };

    //     let mut session_key_2 = [0u8; 32];
    //     hkdf_extract_expand(
    //         k,
    //         b"Pair-Setup-Encrypt-Salt",
    //         b"Pair-Setup-Encrypt-Info",
    //         &mut session_key_2,
    //     )?;

    //     let decrypted_tlv = chacha20_poly1305_decrypt(
    //         &session_key_2,
    //         b"\0\0\0\0PS-Msg06",
    //         &[],
    //         encrypted_tlv_data,
    //         tag_data,
    //     )?;

    //     let accessory_items = tlv8::mapify(tlv8::decode(&decrypted_tlv)?);
    //     let Some(accessory_id_bytes) = accessory_items.get(&tlv8::Tag::Identifier) else {
    //         bail!("Missing accessory ID");
    //     };
    //     let Some(accessory_ltpk_bytes) = accessory_items.get(&tlv8::Tag::PublicKey) else {
    //         bail!("Missing accessory LTPK");
    //     };
    //     let Some(accessory_sig_bytes) = accessory_items.get(&tlv8::Tag::Signature) else {
    //         bail!("Missing accessory signature");
    //     };

    //     self.accessory_ltpk = Some(accessory_ltpk_bytes.to_vec());
    //     let mut accessory_x = [0u8; 32];
    //     hkdf_extract_expand(
    //         k,
    //         b"Pair-Setup-Accessory-Sign-Salt",
    //         b"Pair-Setup-Accessory-Sign-Info",
    //         &mut accessory_x,
    //     )?;

    //     let mut accessory_info = accessory_x.to_vec();
    //     accessory_info.extend_from_slice(accessory_id_bytes);
    //     accessory_info.extend_from_slice(accessory_ltpk_bytes);

    //     ensure!(accessory_ltpk_bytes.len() == 32);
    //     let mut accessory_ltpk_byte_slice = [0u8; 32];
    //     accessory_ltpk_byte_slice.copy_from_slice(&accessory_ltpk_bytes[0..32]);

    //     let verifier = ed25519_dalek::VerifyingKey::from_bytes(&accessory_ltpk_byte_slice)?;
    //     verifier
    //         .verify(
    //             &accessory_info,
    //             &ed25519_dalek::Signature::from_slice(accessory_sig_bytes)?,
    //         )
    //         .context("Accessory signature not verified")?;

    //     debug!("Accessory signature is valid");

    //     let curve_priv = x25519_dalek::EphemeralSecret::random();
    //     let curve_pub = x25519_dalek::PublicKey::from(&curve_priv);
    //     self.verifier_private_key = Some(curve_priv);
    //     self.verifier_public_key = Some(curve_pub);

    //     let state_item = tlv8::Item::new(tlv8::Tag::State, vec![PairingState::M1 as u8]);
    //     let pk_item = tlv8::Item::new(tlv8::Tag::PublicKey, curve_pub.as_bytes().to_vec());
    //     let encoded_tlv = tlv8::encode(&[state_item, pk_item], false);
    //     let encoded_tlv_len_str = encoded_tlv.len().to_string();

    //     let headers = [
    //         ("Content-Type", "application/pairing+tlv8"),
    //         ("Content-Length", &encoded_tlv_len_str),
    //     ];

    //     debug!("pair-verify [1/2]: sending request...");

    //     let (resp_status, resp_body) = self
    //         .post_rtsp_with_resp("pair-verify", &encoded_tlv, Some(&headers))
    //         .await?;

    //     if resp_status == rtsp::StatusCode::Ok {
    //         let Some(body) = resp_body else {
    //             bail!("Failed to process pair-verify M1");
    //         };
    //         Ok(body)
    //     } else {
    //         Err(anyhow!("Pair-verify M1 failed"))
    //     }
    // }

    // 5.7.3 M3: iOS Device -> Accessory - 'Verify Finish Request'
    // async fn pair_verify_m2(
    //     &mut self,
    //     fields: HashMap<tlv8::Tag, Vec<u8>>,
    // ) -> anyhow::Result<Vec<u8>> {
    //     self.sender_state = SenderState::WaitingOnPairVerify2;

    //     let Some(accessory_curve_pub_bytes) = fields.get(&tlv8::Tag::PublicKey) else {
    //         bail!("Public key missing");
    //     };
    //     let Some(accessory_encrypted_field) = fields.get(&tlv8::Tag::EncryptedData) else {
    //         bail!("Encrypted data missing");
    //     };
    //     self.accessory_curve_public = Some(accessory_curve_pub_bytes.clone());

    //     ensure!(accessory_encrypted_field.len() >= TAG_LENGTH);
    //     let encrypted_tlv_data =
    //         &accessory_encrypted_field[..accessory_encrypted_field.len() - TAG_LENGTH];
    //     let auth_tag = &accessory_encrypted_field[accessory_encrypted_field.len() - TAG_LENGTH..];

    //     let Some(priv_param) = self.verifier_private_key.take() else {
    //         bail!("Missing verifier");
    //     };

    //     ensure!(accessory_curve_pub_bytes.len() == 32);
    //     let mut accessory_curve_pub_slice = [0u8; 32];
    //     accessory_curve_pub_slice.copy_from_slice(&accessory_curve_pub_bytes[0..32]);

    //     // Generate the shared secret, SharedSecret, from its Curve25519 secret key and the accessoryʼs
    //     // Curve25519 public key.
    //     let pub_param = x25519_dalek::PublicKey::from(accessory_curve_pub_slice);
    //     let shared_secret = priv_param.diffie_hellman(&pub_param);

    //     //  Derive the symmetric session encryption key, SessionKey, in the same manner as the accessory.
    //     let mut session_key = [0u8; 32];
    //     hkdf_extract_expand(
    //         shared_secret.as_bytes(),
    //         b"Pair-Verify-Encrypt-Salt",
    //         b"Pair-Verify-Encrypt-Info",
    //         &mut session_key,
    //     )?;

    //     self.accessory_shared_secret = Some(shared_secret);

    //     // Verify the 16-byte auth tag, authTag, against the received encryptedData. If this fails, the
    //     // setup process will be aborted and an error will be reported to the user.
    //     // Decrypt the sub-TLV from the received encryptedData.
    //     let decrypted_tlv = chacha20_poly1305_decrypt(
    //         &session_key,
    //         b"\0\0\0\0PV-Msg02",
    //         &[],
    //         encrypted_tlv_data,
    //         auth_tag,
    //     )?;
    //     let accessory_items = tlv8::mapify(tlv8::decode(&decrypted_tlv)?);
    //     let accessory_id_bytes = accessory_items
    //         .get(&tlv8::Tag::Identifier)
    //         .ok_or(anyhow!("Missing accessory ID"))?;
    //     let accessory_sig_bytes = accessory_items
    //         .get(&tlv8::Tag::Signature)
    //         .ok_or(anyhow!("Missing accessory signature"))?;
    //     let verifier_public_key = self
    //         .verifier_public_key
    //         .as_ref()
    //         .ok_or(anyhow!("Missing verifier public key"))?;

    //     // Use the accessoryʼs Pairing Identifier to look up the accessoryʼs long-term public key, AccessoryLTPK,
    //     // in its list of paired accessories. If not found, the setup process will be aborted and an error will
    //     // be reported to the user.
    //     let Some(accessory_ltpk) = self.accessory_ltpk.as_ref() else {
    //         bail!("Missing accessory LTPK");
    //     };
    //     ensure!(accessory_ltpk.len() == 32);
    //     let mut accessory_ltpk_array = [0u8; 32];
    //     accessory_ltpk_array.copy_from_slice(&accessory_ltpk[0..32]);

    //     let accessory_info = [
    //         accessory_curve_pub_bytes.as_slice(),
    //         accessory_id_bytes.as_slice(),
    //         verifier_public_key.as_bytes(),
    //     ]
    //     .concat();
    //     let verifier = ed25519_dalek::VerifyingKey::from_bytes(&accessory_ltpk_array)?;
    //     verifier.verify(
    //         &accessory_info,
    //         &ed25519_dalek::Signature::from_slice(accessory_sig_bytes)?,
    //     )?;
    //     debug!("Accessory signature is valid");

    //     // Construct iOSDeviceInfo by concatenating the following items in order:
    //     //     (a) iOS Deviceʼs Curve25519 public key.
    //     //     (b) iOS Deviceʼs Pairing Identifier, iOSDevicePairingID.
    //     //     (c) Accessoryʼs Curve25519 public key from the received <M2> TLV
    //     let device_info = [
    //         verifier_public_key.as_bytes(),
    //         DEVICE_ID.as_bytes(),
    //         &accessory_curve_pub_slice,
    //     ]
    //     .concat();
    //     // Use Ed25519 to generate iOSDeviceSignature by signing iOSDeviceInfo with its long-term
    //     // secret key, iOSDeviceLTSK.
    //     let device_private_key = self
    //         .device_private_key
    //         .as_ref()
    //         .ok_or(anyhow!("Missing device private key"))?;
    //     let signature =
    //         ed25519_dalek::SigningKey::from_bytes(device_private_key).sign(&device_info);

    //     // Construct a sub-TLV with the following items:
    //     //     kTLVType_Identifier <iOSDevicePairingID>
    //     //     kTLVType_Signature <iOSDeviceSignature>
    //     let identifier_item = tlv8::Item::new(tlv8::Tag::Identifier, DEVICE_ID.as_bytes().to_vec());
    //     let signature_item = tlv8::Item::new(tlv8::Tag::Signature, signature.to_vec());
    //     let sub_tlv = tlv8::encode(&[identifier_item, signature_item], false);

    //     // Encrypt the sub-TLV, encryptedData, and generate the 16-byte auth tag, authTag. This uses the
    //     // ChaCha20-Poly1305 AEAD algorithm with the following parameters:
    //     // encryptedData, authTag = ChaCha20-Poly1305(SessionKey, Nonce=”PV-Msg03”, AAD=<none>, Msg=<Sub-TLV>)
    //     let (encrypted_data, auth_tag) =
    //         chacha20_poly1305_encrypt(&session_key, b"\0\0\0\0PV-Msg03", &[], &sub_tlv)?;

    //     // Construct the request with the following TLV items:
    //     //     kTLVType_State <M3>
    //     //     kTLVType_EncryptedData <encryptedData with authTag appended>
    //     let state_item = tlv8::Item::new(tlv8::Tag::State, vec![PairingState::M3 as u8]);
    //     let encrypted_data_item = tlv8::Item::new(
    //         tlv8::Tag::EncryptedData,
    //         [encrypted_data, auth_tag].concat(),
    //     );
    //     let encoded_response = tlv8::encode(&[state_item, encrypted_data_item], false);
    //     let encoded_response_len_str = encoded_response.len().to_string();

    //     let headers = [
    //         ("Content-Type", "application/pairing+tlv8"),
    //         ("Content-Length", &encoded_response_len_str),
    //     ];

    //     debug!("pair-verify [2/2]: sending request...");

    //     // Send the request to the accessory.
    //     let (resp_status, resp_body) = self
    //         .post_rtsp_with_resp("pair-verify", &encoded_response, Some(&headers))
    //         .await?;

    //     if resp_status == rtsp::StatusCode::Ok {
    //         let Some(body) = resp_body else {
    //             bail!("Failed to process pair-verify M2");
    //         };
    //         Ok(body)
    //     } else {
    //         Err(anyhow!("Pair-verify M2 failed"))
    //     }
    // }

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
        // debug!("Shared key: {:?}", shared_key.as_bytes());
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
        debug!("enc length: {length}, data.len = {}", data.len());

        if data.len() < BLOCK_LENGTH_LENGTH + length + TAG_LENGTH {
            return Ok(None);
        }

        let ciphertext = &data[BLOCK_LENGTH_LENGTH..BLOCK_LENGTH_LENGTH + length];
        debug!("Ciphertext length: {}", ciphertext.len());
        let auth_tag =
            &data[BLOCK_LENGTH_LENGTH + length..BLOCK_LENGTH_LENGTH + length + TAG_LENGTH];

        let nonce = {
            let b = self.in_count.to_le_bytes();
            [0, 0, 0, 0, b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]
        };

        self.in_count += 1;

        debug!("Decrypting with key: {:?}", self.incoming_key);

        // chacha20_poly1305_decrypt(&self.incoming_key, &nonce, &[], ciphertext, auth_tag)?;
        let plaintext = chacha20_poly1305_decrypt(
            &self.incoming_key,
            &nonce,
            &[data[0], data[1]],
            ciphertext,
            auth_tag,
        )?;

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

    async fn pair_verify_start(&mut self) -> anyhow::Result<Vec<u8>> {
        debug!("Starting pair-verify");

        self.sender_state = SenderState::WaitingOnPairVerify1;

        let curve_priv = x25519_dalek::EphemeralSecret::random();
        let curve_pub = x25519_dalek::PublicKey::from(&curve_priv);

        // let mut rng = rand_pcg::Pcg64::new(0xcafef00dd15ea5e5, 0xa02bdbf7bb3c0a7);
        // let mut seed = [0u8; 32];
        // rng.fill_bytes(&mut seed);
        // let ed_priv = ed25519_dalek::SigningKey::from_bytes(&seed);
        // let ed_pub = ed_priv.verifying_key().to_bytes();
        // self.device_private_key = Some(ed_priv.to_bytes());
        // self.device_public_key = Some(ed_pub);

        self.verifier_private_key = Some(curve_priv);
        self.verifier_public_key = Some(curve_pub);

        // self.verifier_private_key = Some(ed_priv);
        // self.verifier_public_key = Some(ed_pub);

        let state_item = tlv8::Item::new(tlv8::Tag::State, vec![PairingState::M1 as u8]);
        // let public_key_item = tlv8::Item::new(tlv8::Tag::PublicKey, ed_pub.to_vec());
        let public_key_item = tlv8::Item::new(tlv8::Tag::PublicKey, curve_pub.to_bytes().to_vec());
        debug!("public len: {}", curve_pub.to_bytes().len());
        let encoded_response = tlv8::encode(&[state_item, public_key_item], false);
        let encoded_response_len_str = encoded_response.len().to_string();

        // let cseq_str = self.rtsp_cseq.to_string(); // #
        // self.rtsp_cseq += 1; // #

        let headers = [
            ("Content-Type", "application/octet-stream"),
            // ("CSeq", &cseq_str), // #
            ("Content-Length", &encoded_response_len_str),
        ];

        // let request = rtsp::Request {
        //     method: rtsp::Method::Post,
        //     path: "/pair-verify",
        //     version: rtsp::Version::Rtsp10,
        //     headers: &headers,
        //     body: Some(&encoded_response),
        // };

        let (resp_status, resp_body) = self
            .post_rtsp_with_resp("pair-verify", &encoded_response, Some(&headers))
            .await?;

        debug!("{resp_status:?}, {resp_body:?}");

        if resp_status != rtsp::StatusCode::Ok {
            bail!("Request failed");
        }

        Ok(resp_body.ok_or(anyhow!("Missing body"))?)

        // let mut encoded_req = Vec::new();
        // request.encode_into(&mut encoded_req);

        // debug!("encoded request:\n{}", hexdump(&encoded_req));

        // let encrypted = self.encrypt_data(&encoded_req)?;

        // debug!("encrypted: {encrypted:?}");

        // match self.stream.as_mut() {
        //     Some(stream) => stream.write_all(&encrypted).await?,
        //     None => bail!("Cannot send request because stream is missing"),
        // }

        // let mut read_buf = Vec::new();
        // let mut tmp_buf = [0u8; 1024];
        // loop {
        //     match self.stream.as_mut() {
        //         Some(stream) => {
        //             let n_read = stream.read(&mut tmp_buf).await?;
        //             read_buf.extend_from_slice(&tmp_buf[0..n_read]);
        //             if n_read < tmp_buf.len() {
        //                 break;
        //             }
        //         }
        //         None => bail!("Cannot send feedback because stream is missing"),
        //     }
        // }

        // debug!("Response: {read_buf:?}");

        // if let Some(plaintext) = self.decrypt_data(&read_buf)? {
        //     debug!("Response:\n{}", hexdump(&plaintext));
        //     return Ok(plaintext);
        // } else {
        //     bail!("Could not decrypt");
        // }
    }

    async fn pair_verify_finish(
        &mut self,
        fields: HashMap<tlv8::Tag, Vec<u8>>,
    ) -> anyhow::Result<Vec<u8>> {
        let Some(accessory_curve_pub_bytes) = fields.get(&tlv8::Tag::PublicKey) else {
            bail!("Public key missing");
        };
        self.accessory_curve_public = Some(accessory_curve_pub_bytes.clone());

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

        let mut session_key = [0u8; 32];
        hkdf_extract_expand(
            shared_secret.as_bytes(),
            b"Pair-Verify-Encrypt-Salt",
            b"Pair-Verify-Encrypt-Info",
            &mut session_key,
        )?;

        self.accessory_shared_secret = Some(shared_secret);

        let Some(accessory_encrypted_field) = fields.get(&tlv8::Tag::EncryptedData) else {
            bail!("Encrypted data missing");
        };

        ensure!(accessory_encrypted_field.len() >= TAG_LENGTH);
        let encrypted_tlv_data =
            &accessory_encrypted_field[..accessory_encrypted_field.len() - TAG_LENGTH];
        let auth_tag = &accessory_encrypted_field[accessory_encrypted_field.len() - TAG_LENGTH..];

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
        let accessory_ltpk = b"transient";
        // let curve_pub = x25519_dalek::PublicKey::from(accessory_ltpk);
        // let Some(accessory_ltpk) = self.accessory_ltpk.as_ref() else {
        //     bail!("Missing accessory LTPK");
        // };
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
            ("Content-Type", "application/pairing+tlv8"),
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

    // async fn pair_verify_finish(
    //     &mut self,
    //     fields: HashMap<tlv8::Tag, Vec<u8>>,
    // ) -> anyhow::Result<()> {
    //     todo!()
    // }

    async fn auth_setup(&mut self) -> anyhow::Result<()> {
        let cseq_str = self.rtsp_cseq.to_string();
        self.rtsp_cseq += 1;

        #[repr(u8)]
        #[allow(dead_code)]
        enum EncryptionType {
            Unencrypted = 0x01,
            Mfi = 0x10,
        }

        let mut body = Vec::new();
        body.push(EncryptionType::Unencrypted as u8);
        body.extend_from_slice(&hex!(
            // https://github.com/owntone/owntone-server/blob/c1db4d914f5cd8e7dbe6c1b6478d68a4c14824af/src/outputs/raop.c#L276
            "59 02 ed e9 0d 4e f2 bd 4c b6 8a 63 30 03 82 07 a9 4d bd 50 d8 aa 46 5b 5d 8c 01 2a 0c 7e 1d 4e"
        ));

        let body_len_string = body.len().to_string();
        let req = rtsp::Request {
            method: rtsp::Method::Post,
            path: "/auth-setup",
            version: rtsp::Version::Rtsp10,
            headers: &[
                ("X-Apple-ProtocolVersion", "1"),
                ("CSeq", &cseq_str),
                ("User-Agent", "AirPlay/381.13"),
                ("Content-Type", "application/octet-stream"),
                ("Content-Length", &body_len_string),
            ],
            body: Some(&body),
        };

        debug!("Sending request: {req:?}");

        let mut encoded_req = Vec::new();
        req.encode_into(&mut encoded_req);

        let encrypted = self.encrypt_data(&encoded_req)?;
        let Some(stream) = self.stream.as_mut() else {
            todo!();
        };

        debug!("Encrypted: {encrypted:?}");

        stream.write_all(&encrypted).await?;

        let mut read_buf = Vec::new();
        let mut tmp_buf = [0u8; 1024];
        loop {
            match self.stream.as_mut() {
                Some(stream) => {
                    let n_read = stream.read(&mut tmp_buf).await?;
                    read_buf.extend_from_slice(&tmp_buf[0..n_read]);
                    if n_read < tmp_buf.len() {
                        break;
                    }
                }
                None => bail!("Cannot send feedback because stream is missing"),
            }
        }

        debug!("{read_buf:?}");

        // stream.wri

        // let (resp_status, resp_body) = self.send_rtsp_request(req).await?;
        // if resp_status != rtsp::StatusCode::Ok {
        //     bail!("Failed to get setup auth, status code: {resp_status:?}");
        // }

        // debug!("{resp_body:?}");

        // let Some(body) = resp_body else {
        //     bail!("Failed to get `/info`: missing body");
        // };

        Ok(())
    }

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
                    let error_code = tlv8::ErrorCode::from(error_bytes[0]);
                    error!("Got error code: {error_code:?}");
                    if let Some(backoff_bytes) = item_map.get(&tlv8::Tag::RetryDelay) {
                        if backoff_bytes.len() <= 8 {
                            let mut full_bytes = [0; 8];
                            full_bytes[0..backoff_bytes.len()].copy_from_slice(backoff_bytes);
                            let backoff_seconds = u64::from_le_bytes(full_bytes);
                            bail!(
                                "Pairing backoff requested, should retry in {backoff_seconds} seconds"
                            );
                        } else {
                            bail!("Invalid number of backoff bytes {}", backoff_bytes.len());
                        }
                    }
                    bail!("Pairing failed with error code {error_code:?}");
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
                    // response_data = self.pair_setup_m4_m5(item_map).await?;
                    self.pair_setup_m4_m5(item_map).await?;

                    self.sender_state = SenderState::ReadyToPlay;
                    // self.set_ciphers()?;
                    self.is_encrypted = true;
                    self.paired = true;

                    // response_data = self.pair_verify_start().await?;
                    // let item_map = tlv8::mapify(tlv8::decode(&response_data)?);
                    // debug!("{item_map:?}");

                    // response_data = self.pair_verify_finish(item_map).await?;
                    // let item_map = tlv8::mapify(tlv8::decode(&response_data)?);
                    // debug!("{item_map:?}");

                    // self.pairing_did_finish().await?;
                    break;
                }
                // SenderState::WaitingOnPairSetup3 if remote_state == PairingState::M6 => {
                SenderState::WaitingOnPairSetup3 => {
                    response_data = self.pair_verify_start().await?;
                }
                SenderState::WaitingOnPairVerify1 if remote_state == PairingState::M2 => {
                    response_data = self.pair_verify_finish(item_map).await?;
                    // response_data = self.pair_verify_m2(item_map).await?;
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
        let body_plist: HashMap<&'static str, plist::Value> =
            HashMap::from([("qualifier", vec!["txtAirPlay".into()].into())]);

        let mut body_writer = std::io::Cursor::new(Vec::new());
        plist::to_writer_binary(&mut body_writer, &body_plist)?;

        let cseq_str = self.rtsp_cseq.to_string();
        self.rtsp_cseq += 1;
        let content_length_str = body_writer.get_ref().len().to_string();
        let req = rtsp::Request {
            method: rtsp::Method::Get,
            path: "/info",
            version: rtsp::Version::Rtsp10,
            headers: &[
                ("X-Apple-ProtocolVersion", "1"),
                ("CSeq", &cseq_str),
                ("User-Agent", "AirPlay/381.13"),
                ("Content-Type", BPLIST_CONTENT_TYPE),
                ("Content-Length", &content_length_str),
            ],
            body: Some(body_writer.get_ref()),
        };

        debug!("Sending request: {req:?}");

        let (resp_status, resp_body) = self.send_rtsp_request(req).await?;
        if resp_status != rtsp::StatusCode::Ok {
            bail!("Failed to get `/info`, status code: {resp_status:?}");
        }

        let Some(body) = resp_body else {
            bail!("Failed to get `/info`: missing body");
        };

        Ok(info_from_plist(&body)?)
    }

    async fn setup(&mut self) -> anyhow::Result<()> {
        // https://github.com/postlund/pyatv/blob/49f9c9e960930276c8fac9fb7696b54a7beb1951/pyatv/protocols/raop/protocols/airplayv2.py#L51
        let body: HashMap<&'static str, plist::Value> = HashMap::from([
            ("deviceID", "AA,BB,CC,DD,EE,FF".into()),
            ("sessionUUID", Uuid::new_v4().to_string().into()),
            // ("sessionUUID", str(uuid4()).upper()),
            // ("timingPort", timing_server_port),
            ("timingPort", 5000.into()),
            ("timingProtocol", "NTP".into()),
            ("isMultiSelectAirPlay", true.into()),
            ("groupContainsGroupLeader", false.into()),
            ("macAddress", "AA,BB,CC,DD,EE,FF".into()),
            ("model", "iPhone14,3".into()),
            ("name", "pyatv".into()),
            ("osBuildVersion", "20F66".into()),
            ("osName", "iPhone OS".into()),
            ("osVersion", "16.5".into()),
            ("senderSupportsRelay", false.into()),
            ("sourceVersion", "690.7.1".into()),
            ("statsCollectionEnabled", false.into()),
        ]);

        let mut writer = std::io::Cursor::new(Vec::new());
        plist::to_writer_binary(&mut writer, &body)?;

        let mut rng = rand_pcg::Pcg64::new(0xcafef00dd15ea5e5, 0xa02bdbf7bb3c0a7);
        let mut session_id = [0; 4];
        rng.fill_bytes(&mut session_id);
        let mut dacp_id = [0; 8];
        rng.fill_bytes(&mut dacp_id);
        let mut active_remote = [0; 4];
        rng.fill_bytes(&mut active_remote);
        let session_id_str = format!("{}", u32::from_be_bytes(session_id));
        let dacp_id_str = format!("{:X}", u64::from_be_bytes(dacp_id));
        let active_remote_str = format!("{}", u32::from_be_bytes(active_remote));

        let path = format!("rtsp://192.168.1.133/{session_id_str}");

        self.rtsp_cseq = 1;
        let cseq_str = self.rtsp_cseq.to_string();
        self.rtsp_cseq += 1;
        let content_length = writer.get_ref().len().to_string();
        let req = rtsp::Request {
            method: rtsp::Method::Setup,
            // path: "rtsp://192.168.1.203/14351957919123295992",
            path: &path,
            // path: "/2182745467221657149",
            version: rtsp::Version::Rtsp10,
            headers: &[
                // ("X-Apple-ProtocolVersion", "1"),
                // ("X-Apple-HKP", "3"),
                // ("X-Apple-HKP", "4"),
                ("CSeq", &cseq_str),
                // ("User-Agent", "AirPlay/381.13"),
                ("User-Agent", "AirPlay/550.10"),
                ("Content-Type", BPLIST_CONTENT_TYPE),
                // ("DACP-ID", "207987F49EDCA9F9"),
                ("DACP-ID", &dacp_id_str),
                // ("Active-Remote", "3307516521"),
                ("Active-Remote", &active_remote_str),
                // ("Client-Instance", "207987F49EDCA9F9"),
                ("Client-Instance", &dacp_id_str),
                ("Content-Length", &content_length),
            ],
            body: Some(writer.get_ref()),
        };

        debug!("Sending request: {req:?}");

        let (resp_status, resp_body) = self.send_rtsp_request(req).await?;
        if resp_status != rtsp::StatusCode::Ok {
            bail!("Failed to setup, status code: {resp_status:?}");
        }

        let read_buf = resp_body.unwrap();

        // let mut encoded_req = Vec::new();
        // req.encode_into(&mut encoded_req);
        // debug!("raw: {encoded_req:?}");

        // let encrypted = self.encrypt_data(&encoded_req)?;

        // debug!("encrypted: {encrypted:?}");

        // match self.stream.as_mut() {
        //     Some(stream) => stream.write_all(&encrypted).await?,
        //     None => bail!("Cannot send request because stream is missing"),
        // }

        // let mut read_buf = Vec::new();
        // let mut tmp_buf = [0u8; 1024];
        // loop {
        //     match self.stream.as_mut() {
        //         Some(stream) => {
        //             let n_read = stream.read(&mut tmp_buf).await?;
        //             read_buf.extend_from_slice(&tmp_buf[0..n_read]);
        //             if n_read < tmp_buf.len() {
        //                 break;
        //             }
        //         }
        //         None => bail!("Cannot send feedback because stream is missing"),
        //     }
        // }

        debug!("Response: {read_buf:?}");

        if let Some(plaintext) = self.decrypt_data(&read_buf)? {
            debug!("Response:\n{}", hexdump(&plaintext));
        } else {
            debug!("Could not decrypt");
        }

        // let Some(body) = resp_body else {
        //     bail!("Failed to get `/info`: missing body");
        // };

        Ok(())
    }

    async fn pair_pin_start(&mut self) -> anyhow::Result<()> {
        let cseq_str = self.rtsp_cseq.to_string();
        self.rtsp_cseq += 1;
        let req = rtsp::Request {
            method: rtsp::Method::Post,
            path: "/pair-pin-start",
            version: rtsp::Version::Rtsp10,
            headers: &[
                ("X-Apple-ProtocolVersion", "1"),
                ("CSeq", &cseq_str),
                ("User-Agent", "AirPlay/381.13"),
                ("Content-Length", "0"),
            ],
            body: None,
        };

        debug!("Sending request: {req:?}");

        let (resp_status, _resp_body) = self.send_rtsp_request(req).await?;
        if resp_status != rtsp::StatusCode::Ok {
            bail!("Failed to setup, status code: {resp_status:?}");
        }

        Ok(())
    }

    async fn inner_work(
        &mut self,
        addrs: Vec<SocketAddr>,
        mut cmd_rx: Receiver<Command>,
    ) -> anyhow::Result<()> {
        self.event_handler
            .connection_state_changed(DeviceConnectionState::Connecting);

        let Some(stream) =
            net_utils::try_connect_tcp(addrs, 5, &mut cmd_rx, |cmd| cmd == Command::Quit).await?
        else {
            debug!("Received Quit command in connect loop");
            self.event_handler
                .connection_state_changed(DeviceConnectionState::Disconnected);
            return Ok(());
        };

        let used_remote_addr = stream.peer_addr()?;
        let local_addr = stream.local_addr().context("Failed to get local address")?;
        self.used_remote_addr = Some(used_remote_addr);

        debug!("Connected to receiver local={local_addr:?} remote={used_remote_addr}");

        self.stream = Some(stream);

        // TODO: get info and make sure SupportsTransientPairing is enabled
        let info = self.fetch_info().await?;
        let Some(features) = info
            .features
            .map(|feats| AirPlayFeatures::from_bits_truncate(feats))
        else {
            bail!("`features` is missing from receiver info");
        };

        let Some(txt) = info
            .txt_air_play
            .map(|data| utils::decode_dns_txt(data.as_ref()))
        else {
            bail!("`txt` records missing from received info");
        };

        debug!("{txt:?}");

        // let Some(status) = info.status_flags.map(|stat| AirPlayStatus::from_bits_truncate(stat)) else {
        //     bail!("`status` is missing from receiver info");
        // };

        // debug!("Receiver status: {status:?}");

        debug!("Features: {features:?}");

        if !features.contains(AirPlayFeatures::SupportsTransientPairing) {
            bail!("Receiver does not support transient pairing");
        }

        debug!("Receiver supports transient pairing");

        // if status.contains(AirPlayStatus::PasswordRequired) {
        if true {
            self.pair_pin_start().await?;
            println!("Enter pin:");
            let pin = "3939";
            // let mut pin = String::new();
            // std::io::stdin().read_line(&mut pin).unwrap();
            // let pin = pin.strip_suffix('\n').unwrap();
            self.password = Some(pin.to_string());
        }

        //     // TODO: what are the different pairing protocols?
        //     if features.contains(AirPlayFeatures::SupportsHKPairingAndAccessControl) {
        //     } else if features.contains(AirPlayFeatures::Authentication4) {
        //     }
        //     debug!("{features:?}");
        // };

        // debug!("status: {:?}", info.status_flags);
        // debug!("info: {info:?}");

        // self.perform_pair(None).await?;
        self.perform_pair(self.password.clone().as_ref().map(|p| p.as_str()))
            .await?;

        if features.contains(AirPlayFeatures::HasUnifiedAdvertiserInfo) {
            self.auth_setup().await?;
        }

        self.event_handler
            .connection_state_changed(DeviceConnectionState::Connected {
                used_remote_addr: used_remote_addr.ip().into(),
                local_addr: local_addr.ip().into(),
            });

        let mut feedback_interval = tokio::time::interval(FEEDBACK_INTERVAL);

        self.setup().await?;

        loop {
            tokio::select! {
                cmd = cmd_rx.recv() => {
                    let cmd = cmd.ok_or(anyhow!("No more commands"))?;
                    debug!("Received command: {cmd:?}");
                    match cmd {
                        Command::Quit => break,
                    }
                }
                // _ = feedback_interval.tick() => {
                //     let info = self.fetch_info().await?;
                //     debug!("{info:?}");
                // }
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
