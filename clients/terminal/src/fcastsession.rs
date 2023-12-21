use std::{sync::{atomic::{AtomicBool, Ordering}, Arc}, collections::VecDeque};

use crate::{models::{PlaybackUpdateMessage, VolumeUpdateMessage, PlaybackErrorMessage, VersionMessage, KeyExchangeMessage, EncryptedMessage, DecryptedMessage}, transport::Transport};
use openssl::{dh::Dh, base64, pkey::{Private, PKey}, symm::{Cipher, Crypter, Mode}, bn::BigNum};
use serde::Serialize;

#[derive(Debug)]
enum SessionState {
    Idle = 0,
    WaitingForLength,
    WaitingForData,
    Disconnected,
}

#[derive(Debug, PartialEq, Copy, Clone)]
pub enum Opcode {
    None = 0,
    Play = 1,
    Pause = 2,
    Resume = 3,
    Stop = 4,
    Seek = 5,
    PlaybackUpdate = 6,
    VolumeUpdate = 7,
    SetVolume = 8,
    PlaybackError = 9,
    SetSpeed = 10,
    Version = 11,
    KeyExchange = 12,
    Encrypted = 13,
    Ping = 14,
    Pong = 15,
    StartEncryption = 16
}

impl Opcode {
    fn from_u8(value: u8) -> Opcode {
        match value {
            0 => Opcode::None,
            1 => Opcode::Play,
            2 => Opcode::Pause,
            3 => Opcode::Resume,
            4 => Opcode::Stop,
            5 => Opcode::Seek,
            6 => Opcode::PlaybackUpdate,
            7 => Opcode::VolumeUpdate,
            8 => Opcode::SetVolume,
            9 => Opcode::PlaybackError,
            10 => Opcode::SetSpeed,
            11 => Opcode::Version,
            12 => Opcode::KeyExchange,
            13 => Opcode::Encrypted,
            14 => Opcode::Ping,
            15 => Opcode::Pong,
            16 => Opcode::StartEncryption,
            _ => panic!("Unknown value: {}", value),
        }
    }
}

const LENGTH_BYTES: usize = 4;
const MAXIMUM_PACKET_LENGTH: usize = 32000;

pub struct FCastSession<'a> {
    buffer: Vec<u8>,
    bytes_read: usize,
    packet_length: usize,
    stream: Box<dyn Transport + 'a>,
    state: SessionState,
    dh: Option<Dh<Private>>,
    public_key: Option<String>,
    aes_key: Option<Vec<u8>>,
    decrypted_messages_queue: VecDeque<DecryptedMessage>,
    encrypted_messages_queue: VecDeque<EncryptedMessage>,
    encryption_started: bool,
    wait_for_encryption: bool
}

impl<'a> FCastSession<'a> {
    pub fn new<T: Transport + 'a>(stream: T, encrypted: bool) -> Result<Self, Box<dyn std::error::Error>> {
        let (dh, public_key) = if encrypted {
            println!("Initialized DH.");
            generate_key_pair()?
        } else {
            (None, None)
        };

        Ok(FCastSession {
            buffer: vec![0; MAXIMUM_PACKET_LENGTH],
            bytes_read: 0,
            packet_length: 0,
            stream: Box::new(stream),
            state: SessionState::Idle,
            wait_for_encryption: dh.is_some(),
            dh,
            public_key,
            aes_key: None,
            decrypted_messages_queue: VecDeque::new(),
            encrypted_messages_queue: VecDeque::new(),
            encryption_started: false
        })
    }
}

impl FCastSession<'_> {
    pub fn send_message<T: Serialize>(&mut self, opcode: Opcode, message: T) -> Result<(), Box<dyn std::error::Error>> {
        let json = serde_json::to_string(&message)?;

        if opcode != Opcode::Encrypted && opcode != Opcode::KeyExchange && opcode != Opcode::StartEncryption {
            if self.encryption_started {
                println!("Sending encrypted with opcode {:?}.", opcode);
                let decrypted_message = DecryptedMessage::new(opcode as u64, Some(json));
                let encrypted_message = encrypt_message(&self.aes_key.as_ref().unwrap(), &decrypted_message)?;
                return self.send_message(Opcode::Encrypted, &encrypted_message)
            } else if self.wait_for_encryption {
                println!("Queued message with opcode {:?} until encryption is established.", opcode);
                let decrypted_message = DecryptedMessage::new(opcode as u64, Some(json));
                self.decrypted_messages_queue.push_back(decrypted_message);
                return Ok(());
            }
        }

        let data = json.as_bytes();
        let size = 1 + data.len();
        let header_size = LENGTH_BYTES + 1;
        let mut header = vec![0u8; header_size];
        header[..LENGTH_BYTES].copy_from_slice(&(size as u32).to_le_bytes());
        header[LENGTH_BYTES] = opcode as u8;
        
        let packet = [header, data.to_vec()].concat();
        println!("Sent {} bytes with (opcode: {:?}, header size: {}, body size: {}, body: {}).", packet.len(), opcode, header_size, data.len(), json);
        self.stream.transport_write(&packet)?;
        Ok(())
    }

    pub fn send_empty(&mut self, opcode: Opcode) -> Result<(), Box<dyn std::error::Error>> {
        if opcode != Opcode::Encrypted && opcode != Opcode::KeyExchange && opcode != Opcode::StartEncryption {
            let decrypted_message = DecryptedMessage::new(opcode as u64, None);
            if self.encryption_started {
                println!("Sending encrypted with opcode {:?}.", opcode);
                let encrypted_message = encrypt_message(&self.aes_key.as_ref().unwrap(), &decrypted_message)?;
                return self.send_message(Opcode::Encrypted, &encrypted_message)
            } else if self.wait_for_encryption {
                println!("Queued message with opcode {:?} until encryption is established.", opcode);
                self.decrypted_messages_queue.push_back(decrypted_message);
                return Ok(());
            }
        }

        let json = String::new();
        let data = json.as_bytes();
        let size = 1 + data.len();
        let mut header = vec![0u8; LENGTH_BYTES + 1];
        header[..LENGTH_BYTES].copy_from_slice(&(size as u32).to_le_bytes());
        header[LENGTH_BYTES] = opcode as u8;
        
        let packet = [header, data.to_vec()].concat();
        self.stream.transport_write(&packet)?;
        Ok(())
    }

    pub fn receive_loop(&mut self, running: &Arc<AtomicBool>, until_queues_are_empty: bool) -> Result<(), Box<dyn std::error::Error>> {
        if let Some(pk) = &self.public_key {
            println!("Sending public key.");
            self.send_message(Opcode::KeyExchange, &KeyExchangeMessage::new(1, pk.clone()))?;        
        }

        println!("Start receiving.");

        self.state = SessionState::WaitingForLength;

        let mut buffer = [0u8; 1024];
        while running.load(Ordering::SeqCst) {
            if until_queues_are_empty && self.are_queues_empty() {
                break;
            }

            let bytes_read = self.stream.transport_read(&mut buffer)?;
            self.process_bytes(&buffer[..bytes_read])?;
        }

        self.state = SessionState::Idle;
        Ok(())
    }

    fn process_bytes(&mut self, received_bytes: &[u8]) -> Result<(), Box<dyn std::error::Error>> {
        if received_bytes.is_empty() {
            return Ok(());
        }

        println!("{} bytes received", received_bytes.len());

        match self.state {
            SessionState::WaitingForLength => self.handle_length_bytes(received_bytes)?,
            SessionState::WaitingForData => self.handle_packet_bytes(received_bytes)?,
            _ => println!("Data received is unhandled in current session state {:?}", self.state),
        }

        Ok(())
    }


    fn handle_length_bytes(&mut self, received_bytes: &[u8]) -> Result<(), Box<dyn std::error::Error>> {
        let bytes_to_read = std::cmp::min(LENGTH_BYTES, received_bytes.len());
        let bytes_remaining = received_bytes.len() - bytes_to_read;
        self.buffer[self.bytes_read..self.bytes_read + bytes_to_read]
            .copy_from_slice(&received_bytes[..bytes_to_read]);
        self.bytes_read += bytes_to_read;

        println!("handleLengthBytes: Read {} bytes from packet", bytes_to_read);

        if self.bytes_read >= LENGTH_BYTES {
            self.state = SessionState::WaitingForData;
            self.packet_length = u32::from_le_bytes(self.buffer[..LENGTH_BYTES].try_into()?) as usize;
            self.bytes_read = 0;

            println!("Packet length header received from: {}", self.packet_length);

            if self.packet_length > MAXIMUM_PACKET_LENGTH {
                println!("Maximum packet length is 32kB, killing stream: {}", self.packet_length);

                self.stream.transport_shutdown()?;
                self.state = SessionState::Disconnected;
                return Err(format!("Stream killed due to packet length ({}) exceeding maximum 32kB packet size.", self.packet_length).into());
            }
    
            if bytes_remaining > 0 {
                println!("{} remaining bytes pushed to handlePacketBytes", bytes_remaining);

                self.handle_packet_bytes(&received_bytes[bytes_to_read..])?;
            }
        }

        Ok(())
    }

    fn handle_packet_bytes(&mut self, received_bytes: &[u8]) -> Result<(), Box<dyn std::error::Error>> {
        let bytes_to_read = std::cmp::min(self.packet_length, received_bytes.len());
        let bytes_remaining = received_bytes.len() - bytes_to_read;
        self.buffer[self.bytes_read..self.bytes_read + bytes_to_read]
            .copy_from_slice(&received_bytes[..bytes_to_read]);
        self.bytes_read += bytes_to_read;
    
        println!("handlePacketBytes: Read {} bytes from packet", bytes_to_read);
    
        if self.bytes_read >= self.packet_length {           
            println!("Packet finished receiving of {} bytes.", self.packet_length);
            self.handle_next_packet()?;

            self.state = SessionState::WaitingForLength;
            self.packet_length = 0;
            self.bytes_read = 0;
    
            if bytes_remaining > 0 {
                println!("{} remaining bytes pushed to handleLengthBytes", bytes_remaining);
                self.handle_length_bytes(&received_bytes[bytes_to_read..])?;
            }
        }

        Ok(())
    }

    fn handle_next_packet(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        println!("Processing packet of {} bytes", self.bytes_read);
    
        let opcode = Opcode::from_u8(self.buffer[0]);
        let packet_length = self.packet_length;
        let body = if packet_length > 1 {
            Some(std::str::from_utf8(&self.buffer[1..packet_length])?.to_string())
        } else {
            None
        };
    
        println!("Received body: {:?}", body);
        self.handle_packet(opcode, body)
    }

    fn handle_packet(&mut self, opcode: Opcode, body: Option<String>) -> Result<(), Box<dyn std::error::Error>> {
        println!("Received message with opcode {:?}.", opcode);

        match opcode {
            Opcode::PlaybackUpdate => {
                if let Some(body_str) = body {
                    if let Ok(playback_update_msg) = serde_json::from_str::<PlaybackUpdateMessage>(body_str.as_str()) {
                        println!("Received playback update {:?}", playback_update_msg);
                    } else {
                        println!("Received playback update with malformed body.");
                    }
                } else {
                    println!("Received playback update with no body.");
                }
            }
            Opcode::VolumeUpdate => {
                if let Some(body_str) = body {
                    if let Ok(volume_update_msg) = serde_json::from_str::<VolumeUpdateMessage>(body_str.as_str()) {
                        println!("Received volume update {:?}", volume_update_msg);
                    } else {
                        println!("Received volume update with malformed body.");
                    }
                } else {
                    println!("Received volume update with no body.");
                }
            }
            Opcode::PlaybackError => {
                if let Some(body_str) = body {
                    if let Ok(playback_error_msg) = serde_json::from_str::<PlaybackErrorMessage>(body_str.as_str()) {
                        println!("Received playback error {:?}", playback_error_msg);
                    } else {
                        println!("Received playback error with malformed body.");
                    }
                } else {
                    println!("Received playback error with no body.");
                }
            }
            Opcode::Version => {
                if let Some(body_str) = body {
                    if let Ok(version_msg) = serde_json::from_str::<VersionMessage>(body_str.as_str()) {
                        println!("Received version {:?}", version_msg);
                    } else {
                        println!("Received version with malformed body.");
                    }
                } else {
                    println!("Received version with no body.");
                }
            }
            Opcode::KeyExchange => {
                if let Some(body_str) = body {
                    match serde_json::from_str::<KeyExchangeMessage>(body_str.as_str()) {
                        Ok(key_exchange_message) => {
                            if let Some(dh) = &self.dh {
                                println!("Received key exchange message {:?}", key_exchange_message);
                                self.aes_key = Some(compute_shared_secret(dh, &key_exchange_message)?);
                                self.send_empty(Opcode::StartEncryption)?;
    
                                println!("Processing queued encrypted messages to handle.");
                                while let Some(encrypted_message) = self.encrypted_messages_queue.pop_front() {
                                    let decrypted_message = decrypt_message(&self.aes_key.as_ref().unwrap(), &encrypted_message)?;
                                    self.handle_packet(Opcode::from_u8(decrypted_message.opcode as u8), decrypted_message.message)?;
                                }
                            } else {
                                println!("Received key exchange message while encryption is diabled {:?}", key_exchange_message);
                            }
                        },
                        Err(e) => println!("Received key exchange with malformed body: {}.", e)
                    };
                } else {
                    println!("Received key exchange with no body.");
                }
            }
            Opcode::Encrypted => {
                if let Some(body_str) = body {
                    if let Ok(encrypted_message) = serde_json::from_str::<EncryptedMessage>(body_str.as_str()) {
                        println!("Received encrypted message {:?}", encrypted_message);
                        
                        if self.aes_key.is_some() {
                            println!("Decrypting and handling encrypted message.");
                            let decrypted_message = decrypt_message(&self.aes_key.as_ref().unwrap(), &encrypted_message)?;
                            self.handle_packet(Opcode::from_u8(decrypted_message.opcode as u8), decrypted_message.message)?;
                        } else {
                            println!("Queued encrypted message until encryption is established.");
                            self.encrypted_messages_queue.push_back(encrypted_message);
                            
                            if self.encrypted_messages_queue.len() > 15 {
                                self.encrypted_messages_queue.pop_front();
                            }
                        }
                    } else {
                        println!("Received encrypted with malformed body.");
                    }
                } else {
                    println!("Received encrypted with no body.");
                }
            }
            Opcode::Ping => {
                println!("Received ping");
                self.send_empty(Opcode::Pong)?;
                println!("Sent pong");
            }
            Opcode::StartEncryption => {
                self.encryption_started = true;

                println!("Processing queued decrypted messages to send.");    
                while let Some(decrypted_message) = self.decrypted_messages_queue.pop_front() {
                    let encrypted_message = encrypt_message(&self.aes_key.as_ref().unwrap(), &decrypted_message)?;
                    self.send_message(Opcode::Encrypted, &encrypted_message)?;
                }
            }
            _ => {
                println!("Error handling packet");
            }
        }

        Ok(())
    }

    fn are_queues_empty(&self) -> bool {
        return self.decrypted_messages_queue.is_empty() && self.encrypted_messages_queue.is_empty();
    }

    pub fn shutdown(&mut self) -> Result<(), std::io::Error> {
        return self.stream.transport_shutdown();
    }
}

fn generate_key_pair() -> Result<(Option<Dh<Private>>, Option<String>), Box<dyn std::error::Error>> {
    //modp14
    let p = "ffffffffffffffffc90fdaa22168c234c4c6628b80dc1cd129024e088a67cc74020bbea63b139b22514a08798e3404ddef9519b3cd3a431b302b0a6df25f14374fe1356d6d51c245e485b576625e7ec6f44c42e9a637ed6b0bff5cb6f406b7edee386bfb5a899fa5ae9f24117c4b1fe649286651ece45b3dc2007cb8a163bf0598da48361c55d39a69163fa8fd24cf5f83655d23dca3ad961c62f356208552bb9ed529077096966d670c354e4abc9804f1746c08ca18217c32905e462e36ce3be39e772c180e86039b2783a2ec07a28fb5c55df06f4c52c9de2bcbf6955817183995497cea956ae515d2261898fa051015728e5a8aacaa68ffffffffffffffff";
    let g = "2";

    let v = Dh::from_pqg(BigNum::from_hex_str(p)?, None, BigNum::from_hex_str(g)?)?.generate_key()?;

    let private = v.private_key().to_owned()?;    
    let dh2 = Dh::from_pqg(BigNum::from_hex_str(p)?, None, BigNum::from_hex_str(g)?)?.set_private_key(private)?;
    let pkey = PKey::from_dh(dh2)?;
    let public_key_der = pkey.public_key_to_der()?;
    let public_key_base64 = base64::encode_block(public_key_der.as_ref());

    Ok((Some(v), Some(public_key_base64)))
}

fn encrypt_message(aes_key: &Vec<u8>, decrypted_message: &DecryptedMessage) -> Result<EncryptedMessage, Box<dyn std::error::Error>> {
    let cipher = Cipher::aes_256_cbc();
    let iv_len = cipher.iv_len().ok_or("Cipher does not support IV")?;
    let mut iv = vec![0; iv_len];
    openssl::rand::rand_bytes(&mut iv)?;

    let mut crypter = Crypter::new(
        cipher,
        Mode::Encrypt,
        aes_key,
        Some(&iv)
    )?;
    crypter.pad(true);

    let json = serde_json::to_string(decrypted_message)?;
    let mut ciphertext = vec![0; json.len() + cipher.block_size()];
    let count = crypter.update(json.as_bytes(), &mut ciphertext)?;
    let rest = crypter.finalize(&mut ciphertext[count..])?;
    ciphertext.truncate(count + rest);

    Ok(EncryptedMessage::new(1, Some(base64::encode_block(&iv)), base64::encode_block(&ciphertext)))
}

fn decrypt_message(aes_key: &Vec<u8>, encrypted_message: &EncryptedMessage) -> Result<DecryptedMessage, Box<dyn std::error::Error>> {
    if encrypted_message.iv.is_none() {
        return Err("IV is required for decryption.".into());
    }

    let cipher = Cipher::aes_256_cbc();
    let iv = base64::decode_block(&encrypted_message.iv.as_ref().unwrap())?;
    let ciphertext = base64::decode_block(&encrypted_message.blob)?;

    let mut crypter = Crypter::new(
        cipher,
        Mode::Decrypt,
        aes_key,
        Some(&iv)
    )?;
    crypter.pad(true);

    let mut plaintext = vec![0; ciphertext.len() + cipher.block_size()];
    let count = crypter.update(&ciphertext, &mut plaintext)?;
    let rest = crypter.finalize(&mut plaintext[count..])?;
    plaintext.truncate(count + rest);

    let decrypted_str = String::from_utf8(plaintext)?;
    Ok(serde_json::from_str(&decrypted_str)?)
}

fn compute_shared_secret(dh: &Dh<Private>, key_exchange_message: &KeyExchangeMessage) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let peer_public_key_der = base64::decode_block(&key_exchange_message.public_key)?;
    let peer_public_key = PKey::public_key_from_der(&peer_public_key_der)?;
    let peer_dh = peer_public_key.dh()?;
    let peer_pub_key = peer_dh.public_key();
    let shared_secret = dh.compute_key(&peer_pub_key)?;                    
    let digest = openssl::hash::hash(openssl::hash::MessageDigest::sha256(), &shared_secret)?.to_vec();
    Ok(digest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use openssl::base64;

    #[test]
    fn test_dh_encryption_self() {
        let (key_pair1, public_key1) = generate_key_pair().unwrap();
        let (key_pair2, public_key2) = generate_key_pair().unwrap();

        let aes_key1 = compute_shared_secret(&key_pair1.unwrap(), &KeyExchangeMessage::new(1, public_key2.unwrap())).unwrap();
        let aes_key2 = compute_shared_secret(&key_pair2.unwrap(), &KeyExchangeMessage::new(1, public_key1.unwrap())).unwrap();

        assert_eq!(aes_key1, aes_key2);

        let message = DecryptedMessage {
            opcode: 1,
            message: Some(r#"{"type": "text/html"}"#.to_string()),
        };
        let encrypted_message = encrypt_message(&aes_key1, &message).unwrap();
        let decrypted_message = decrypt_message(&aes_key1, &encrypted_message).unwrap();

        assert_eq!(message.opcode, decrypted_message.opcode);
        assert_eq!(message.message, decrypted_message.message);
    }


    #[test]
    fn test_dh_encryption_known() {
        let private_key1 = base64::decode_block("MIIDJwIBADCCAhgGCSqGSIb3DQEDATCCAgkCggEBAJVHXPXZPllsP80dkCrdAvQn9fPHIQMTu0X7TVuy5f4cvWeM1LvdhMmDa+HzHAd3clrrbC/Di4X0gHb6drzYFGzImm+y9wbdcZiYwgg9yNiW+EBi4snJTRN7BUqNgJatuNUZUjmO7KhSoK8S34Pkdapl1OwMOKlWDVZhGG/5i5/J62Du6LAwN2sja8c746zb10/WHB0kdfowd7jwgEZ4gf9+HKVv7gZteVBq3lHtu1RDpWOSfbxLpSAIZ0YXXIiFkl68ZMYUeQZ3NJaZDLcU7GZzBOJh+u4zs8vfAI4MP6kGUNl9OQnJJ1v0rIb/yz0D5t/IraWTQkLdbTvMoqQGywsCggEAQt67naWz2IzJVuCHh+w/Ogm7pfSLiJp0qvUxdKoPvn48W4/NelO+9WOw6YVgMolgqVF/QBTTMl/Hlivx4Ek3DXbRMUp2E355Lz8NuFnQleSluTICTweezy7wnHl0UrB3DhNQeC7Vfd95SXnc7yPLlvGDBhllxOvJPJxxxWuSWVWnX5TMzxRJrEPVhtC+7kMlGwsihzSdaN4NFEQD8T6AL0FG2ILgV68ZtvYnXGZ2yPoOPKJxOjJX/Rsn0GOfaV40fY0c+ayBmibKmwTLDrm3sDWYjRW7rGUhKlUjnPx+WPrjjXJQq5mR/7yXE0Al/ozgTEOZrZZWm+kaVG9JeGk8egSCAQQCggEAECNvEczf0y6IoX/IwhrPeWZ5IxrHcpwjcdVAuyZQLLlOq0iqnYMFcSD8QjMF8NKObfZZCDQUJlzGzRsG0oXsWiWtmoRvUZ9tQK0j28hDylpbyP00Bt9NlMgeHXkAy54P7Z2v/BPCd3o23kzjgXzYaSRuCFY7zQo1g1IQG8mfjYjdE4jjRVdVrlh8FS8x4OLPeglc+cp2/kuyxaVEfXAG84z/M8019mRSfdczi4z1iidPX6HgDEEWsN42Ud60mNKy5jsQpQYkRdOLmxR3+iQEtGFjdzbVhVCUr7S5EORU9B1MOl5gyPJpjfU3baOqrg6WXVyTvMDaA05YEnAHQNOOfA==").unwrap();
        let key_exchange_message_2 = KeyExchangeMessage::new(1, "MIIDJTCCAhgGCSqGSIb3DQEDATCCAgkCggEBAJVHXPXZPllsP80dkCrdAvQn9fPHIQMTu0X7TVuy5f4cvWeM1LvdhMmDa+HzHAd3clrrbC/Di4X0gHb6drzYFGzImm+y9wbdcZiYwgg9yNiW+EBi4snJTRN7BUqNgJatuNUZUjmO7KhSoK8S34Pkdapl1OwMOKlWDVZhGG/5i5/J62Du6LAwN2sja8c746zb10/WHB0kdfowd7jwgEZ4gf9+HKVv7gZteVBq3lHtu1RDpWOSfbxLpSAIZ0YXXIiFkl68ZMYUeQZ3NJaZDLcU7GZzBOJh+u4zs8vfAI4MP6kGUNl9OQnJJ1v0rIb/yz0D5t/IraWTQkLdbTvMoqQGywsCggEAQt67naWz2IzJVuCHh+w/Ogm7pfSLiJp0qvUxdKoPvn48W4/NelO+9WOw6YVgMolgqVF/QBTTMl/Hlivx4Ek3DXbRMUp2E355Lz8NuFnQleSluTICTweezy7wnHl0UrB3DhNQeC7Vfd95SXnc7yPLlvGDBhllxOvJPJxxxWuSWVWnX5TMzxRJrEPVhtC+7kMlGwsihzSdaN4NFEQD8T6AL0FG2ILgV68ZtvYnXGZ2yPoOPKJxOjJX/Rsn0GOfaV40fY0c+ayBmibKmwTLDrm3sDWYjRW7rGUhKlUjnPx+WPrjjXJQq5mR/7yXE0Al/ozgTEOZrZZWm+kaVG9JeGk8egOCAQUAAoIBAGlL9EYsrFz3I83NdlwhM241M+M7PA9P5WXgtdvS+pcalIaqN2IYdfzzCUfye7lchVkT9A2Y9eWQYX0OUhmjf8PPKkRkATLXrqO5HTsxV96aYNxMjz5ipQ6CaErTQaPLr3OPoauIMPVVI9zM+WT0KOGp49YMyx+B5rafT066vOVbF/0z1crq0ZXxyYBUv135rwFkIHxBMj5bhRLXKsZ2G5aLAZg0DsVam104mgN/v75f7Spg/n5hO7qxbNgbvSrvQ7Ag/rMk5T3sk7KoM23Qsjl08IZKs2jjx21MiOtyLqGuCW6GOTNK4yEEDF5gA0K13eXGwL5lPS0ilRw+Lrw7cJU=".to_string());

        let private_key = PKey::private_key_from_der(&private_key1).unwrap();
        let dh = private_key.dh().unwrap();

        let aes_key1 = compute_shared_secret(&dh, &key_exchange_message_2).unwrap();
        assert_eq!(base64::encode_block(&aes_key1), "vI5LGE625zGEG350ggkyBsIAXm2y4sNohiPcED1oAEE=");

        let message = DecryptedMessage {
            opcode: 1,
            message: Some(r#"{"type": "text/html"}"#.to_string()),
        };
        let encrypted_message = encrypt_message(&aes_key1, &message).unwrap();
        let decrypted_message = decrypt_message(&aes_key1, &encrypted_message).unwrap();

        assert_eq!(message.opcode, decrypted_message.opcode);
        assert_eq!(message.message, decrypted_message.message);
    }

    #[test]
    fn test_decrypt_message_known() {
        let encrypted_message_json = r#"{"version":1,"iv":"C4H70VC5FWrNtkty9/cLIA==","blob":"K6/N7JMyi1PFwKhU0mFj7ZJmd/tPp3NCOMldmQUtDaQ7hSmPoIMI5QNMOj+NFEiP4qTgtYp5QmBPoQum6O88pA=="}"#;
        let encrypted_message: EncryptedMessage = serde_json::from_str(encrypted_message_json).unwrap();
        let aes_key_base64 = "+hr9Jg8yre7S9WGUohv2AUSzHNQN514JPh6MoFAcFNU=";
        let aes_key = base64::decode_block(aes_key_base64).unwrap();

        let decrypted_message = decrypt_message(&aes_key, &encrypted_message).unwrap();
        assert_eq!(1, decrypted_message.opcode);
        assert_eq!("{\"container\":\"text/html\"}", decrypted_message.message.unwrap());
    }

    #[test]
    fn test_aes_key_generation() {
        let cases = vec![
            (
                // Public other
                String::from("MIIBHzCBlQYJKoZIhvcNAQMBMIGHAoGBAP//////////yQ/aoiFowjTExmKLgNwc0SkCTgiKZ8x0Agu+pjsTmyJRSgh5jjQE3e+VGbPNOkMbMCsKbfJfFDdP4TVtbVHCReSFtXZiXn7G9ExC6aY37WsL/1y29Aa37e44a/taiZ+lrp8kEXxLH+ZJKGZR7OZTgf//////////AgECA4GEAAKBgEnOS0oHteVA+3kND3u4yXe7GGRohy1LkR9Q5tL4c4ylC5n4iSwWSoIhcSIvUMWth6KAhPhu05sMcPY74rFMSS2AGTNCdT/5KilediipuUMdFVvjGqfNMNH1edzW5mquIw3iXKdfQmfY/qxLTI2wccyDj4hHFhLCZL3Y+shsm3KF"),
                // Private self
                String::from("MIIBIQIBADCBlQYJKoZIhvcNAQMBMIGHAoGBAP//////////yQ/aoiFowjTExmKLgNwc0SkCTgiKZ8x0Agu+pjsTmyJRSgh5jjQE3e+VGbPNOkMbMCsKbfJfFDdP4TVtbVHCReSFtXZiXn7G9ExC6aY37WsL/1y29Aa37e44a/taiZ+lrp8kEXxLH+ZJKGZR7OZTgf//////////AgECBIGDAoGAeo/ceIeH8Jt1ZRNKX5aTHkMi23GCV1LtcS2O6Tktn9k8DCv7gIoekysQUhMyWtR+MsZlq2mXjr1JFpAyxl89rqoEPU6QDsGe9q8R4O8eBZ2u+48mkUkGSh7xPGRQUBvmhH2yk4hIEA8aK4BcYi1OTsCZtmk7pQq+uaFkKovD/8M="),
                // Expected AES key
                String::from("7dpl1/6KQTTooOrFf2VlUOSqgrFHi6IYxapX0IxFfwk="),
            ),
            (
                // Public other
                String::from("MIIBHzCBlQYJKoZIhvcNAQMBMIGHAoGBAP//////////yQ/aoiFowjTExmKLgNwc0SkCTgiKZ8x0Agu+pjsTmyJRSgh5jjQE3e+VGbPNOkMbMCsKbfJfFDdP4TVtbVHCReSFtXZiXn7G9ExC6aY37WsL/1y29Aa37e44a/taiZ+lrp8kEXxLH+ZJKGZR7OZTgf//////////AgECA4GEAAKBgGvIlCP/S+xpAuNEHSn4cEDOL1esUf+uMuY2Kp5J10a7HGbwzNd+7eYsgEc4+adddgB7hJgTvjsGg7lXUhHQ7WbfbCGgt7dbkx8qkic6Rgq4f5eRYd1Cgidw4MhZt7mEIOKrHweqnV6B9rypbXjbqauc6nGgtwx+Gvl6iLpVATRK"),
                // Private self
                String::from("MIIBIQIBADCBlQYJKoZIhvcNAQMBMIGHAoGBAP//////////yQ/aoiFowjTExmKLgNwc0SkCTgiKZ8x0Agu+pjsTmyJRSgh5jjQE3e+VGbPNOkMbMCsKbfJfFDdP4TVtbVHCReSFtXZiXn7G9ExC6aY37WsL/1y29Aa37e44a/taiZ+lrp8kEXxLH+ZJKGZR7OZTgf//////////AgECBIGDAoGAMXmiIgWyutbaO+f4UiMAb09iVVSCI6Lb6xzNyD2MpUZyk4/JOT04Daj4JeCKFkF1Fq79yKhrnFlXCrF4WFX00xUOXb8BpUUUH35XG5ApvolQQLL6N0om8/MYP4FK/3PUxuZAJz45TUsI/v3u6UqJelVTNL83ltcFbZDIfEVftRA="),
                // Expected AES key
                String::from("a2tUSxnXifKohfNocAQHkAlPffDv6ReihJ7OojBGt0Q=")
            )
        ];

        for case in cases {
            let private_self_key = base64::decode_block(&case.1).expect("Invalid base64 for private self key");
            let expected_aes_key = base64::decode_block(&case.2).expect("Invalid base64 for expected AES key");

            let private_key = PKey::private_key_from_der(&private_self_key).expect("Failed to create private key");
            let dh = private_key.dh().expect("Failed to create DH from private key");
            let key_exchange_message = KeyExchangeMessage::new(1, case.0);

            let aes_key = compute_shared_secret(&dh, &key_exchange_message).expect("Failed to compute shared secret");
            let aes_key_base64 = base64::encode_block(&aes_key);

            assert_eq!(aes_key_base64, base64::encode_block(&expected_aes_key), "AES keys do not match");
        }
    }
}