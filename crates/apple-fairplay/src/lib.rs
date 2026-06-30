//! Pure-Rust port of [PlayFair](https://github.com/EstebanKubata/playfair).

#![allow(unused_assignments, unused_parens)]

mod garble;
mod md5;

mod tables {
    #![cfg_attr(rustfmt, rustfmt::skip)]

    pub static MESSAGE_KEY: &[u8] = &include!("../data/message_key.array");
    pub static MESSAGE_IV: &[u8] = &include!("../data/message_iv.array");

    pub static Z_KEY: &[u8] = &[
        0x1a, 0x64, 0xf9, 0x60, 0x6c, 0xe3, 0x01, 0xa9, 0x54, 0x48, 0x1b, 0xd4, 0xab, 0x81, 0xfc, 0xc6,
    ];

    pub static X_KEY: &[u8] = &[
        0x8e, 0xba, 0x07, 0xcc, 0xb6, 0x5a, 0xf6, 0x20, 0x33, 0xcf, 0xf8, 0x42, 0xe5, 0xd5, 0x5a, 0x7d,
    ];

    pub static T_KEY: &[u8] = &[
        0xd0, 0x04, 0xa9, 0x61, 0x6b, 0xa4, 0x00, 0x87, 0x68, 0x8b, 0x5f, 0x15, 0x15, 0x35, 0xd9, 0xa9,
    ];

    pub static TABLE_S5: &[u32] = &include!("../data/table_s5.array");
    pub static TABLE_S6: &[u32] = &include!("../data/table_s6.array");
    pub static TABLE_S7: &[u32] = &include!("../data/table_s7.array");
    pub static TABLE_S8: &[u32] = &include!("../data/table_s8.array");
    pub static TABLE_S9: &[u32] = &include!("../data/table_s9.array");
    pub static TABLE_S1: &[u8] = &include!("../data/table_s1.array");
    pub static TABLE_S2: &[u8] = &include!("../data/table_s2.array");
    pub static TABLE_S3: &[u8] = &include!("../data/table_s3.array");
    pub static TABLE_S4: &[u8] = &include!("../data/table_s4.array");
    pub static TABLE_S10: &[u8] = &include!("../data/table_s10.array");

    pub static INDEX_MANGLE: &[u8] = &[
        0x01, 0x02, 0x04, 0x08, 0x10, 0x20, 0x40, 0x80, 0x1b, 0x36, 0x6c,
    ];

    pub static INITIAL_SESSION_KEY: &[u8] = &[
        0xdc, 0xdc, 0xf3, 0xb9, 0x0b, 0x74, 0xdc, 0xfb, 0x86, 0x7f, 0xf7, 0x60, 0x16, 0x72, 0x90, 0x51,
    ];

    pub static STATIC_SOURCE_1: &[u8] = &[
        0xfa, 0x9c, 0xad, 0x4d, 0x4b, 0x68, 0x26, 0x8c, 0x7f, 0xf3, 0x88, 0x99, 0xde, 0x92, 0x2e, 0x95,
        0x1e,
    ];

    pub static STATIC_SOURCE_2: &[u8] = &[
        0xec, 0x4e, 0x27, 0x5e, 0xfd, 0xf2, 0xe8, 0x30, 0x97, 0xae, 0x70, 0xfb, 0xe0, 0x00, 0x3f, 0x1c,
        0x39, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x09, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    ];

    pub static DEFAULT_SAP: &[u8] = &include!("../data/default_sap.array");
    pub static REPLY_MESSAGE: &[u8] = &include!("../data/reply_message.array");

    pub static FP_HEADER: &[u8] = &[
        0x46, 0x50, 0x4c, 0x59, 0x03, 0x01, 0x04, 0x00, 0x00, 0x00, 0x00, 0x14,
    ];
}

use anyhow::{Result, bail};

/// FairPlay session state across the `/fp-setup` handshake.
#[derive(Debug, Clone)]
pub struct FairPlay {
    keymsg: [u8; 164],
    have_keymsg: bool,
}

impl Default for FairPlay {
    fn default() -> Self {
        Self::new()
    }
}

impl FairPlay {
    pub fn new() -> Self {
        Self {
            keymsg: [0u8; 164],
            have_keymsg: false,
        }
    }

    /// `/fp-setup` stage 1: 16-byte request -> 142-byte reply.
    pub fn setup(&mut self, req: &[u8]) -> Result<[u8; 142]> {
        if req.len() < 16 || req[4] != 0x03 {
            bail!("unsupported fairplay setup request");
        }
        let mode = req[14] as usize;
        if mode >= 4 {
            bail!("invalid fairplay setup mode {mode}");
        }
        let mut res = [0u8; 142];
        res.copy_from_slice(&tables::REPLY_MESSAGE[mode * 142..mode * 142 + 142]);
        self.have_keymsg = false;
        Ok(res)
    }

    /// `/fp-setup` stage 2: 164-byte request -> 32-byte reply. Stashes the key
    /// message used later by [`decrypt`](Self::decrypt).
    pub fn handshake(&mut self, req: &[u8]) -> Result<[u8; 32]> {
        if req.len() < 164 || req[4] != 0x03 {
            bail!("unsupported fairplay handshake request");
        }
        self.keymsg.copy_from_slice(&req[..164]);
        self.have_keymsg = true;

        let mut res = [0u8; 32];
        res[..12].copy_from_slice(tables::FP_HEADER);
        res[12..32].copy_from_slice(&req[144..164]);
        Ok(res)
    }

    /// Decrypt the 72-byte `ekey` from `SETUP` into the 16-byte AES key.
    pub fn decrypt(&self, input: &[u8]) -> Result<[u8; 16]> {
        if !self.have_keymsg {
            bail!("fairplay handshake not completed");
        }
        if input.len() < 72 {
            bail!("fairplay ekey too short");
        }
        let mut out = [0u8; 16];
        playfair_decrypt(&self.keymsg, input, &mut out);
        Ok(out)
    }
}

// ---------------------------------------------------------------------------
// playfair core (port of omg_hax.c)
// ---------------------------------------------------------------------------

fn rd_le(buf: &[u8], word: usize) -> u32 {
    let o = word * 4;
    u32::from_le_bytes([buf[o], buf[o + 1], buf[o + 2], buf[o + 3]])
}
fn wr_le(buf: &mut [u8], word: usize, v: u32) {
    let o = word * 4;
    buf[o..o + 4].copy_from_slice(&v.to_le_bytes());
}

fn z_xor(input: &[u8], output: &mut [u8], blocks: usize) {
    for j in 0..blocks {
        for i in 0..16 {
            output[j * 16 + i] = input[j * 16 + i] ^ tables::Z_KEY[i];
        }
    }
}
fn x_xor(input: &[u8], output: &mut [u8], blocks: usize) {
    for j in 0..blocks {
        for i in 0..16 {
            output[j * 16 + i] = input[j * 16 + i] ^ tables::X_KEY[i];
        }
    }
}

/// `&table_s1[((31*i) % 0x28) << 8 ..]`
fn s1_base(i: usize) -> usize {
    ((31 * i) % 0x28) << 8
}
/// `&table_s2[(97*i % 144) << 8 ..]`
fn s2_base(i: usize) -> usize {
    ((97 * i) % 144) << 8
}
/// `&table_s4[((71*i) % 144) << 8 ..]`
fn s4_base(i: usize) -> usize {
    ((71 * i) % 144) << 8
}

fn permute_block_1(block: &mut [u8]) {
    let t = &tables::TABLE_S3;
    block[0] = t[block[0] as usize];
    block[4] = t[0x400 + block[4] as usize];
    block[8] = t[0x800 + block[8] as usize];
    block[12] = t[0xc00 + block[12] as usize];

    let mut tmp = block[13];
    block[13] = t[0x100 + block[9] as usize];
    block[9] = t[0xd00 + block[5] as usize];
    block[5] = t[0x900 + block[1] as usize];
    block[1] = t[0x500 + tmp as usize];

    tmp = block[2];
    block[2] = t[0xa00 + block[10] as usize];
    block[10] = t[0x200 + tmp as usize];
    tmp = block[6];
    block[6] = t[0xe00 + block[14] as usize];
    block[14] = t[0x600 + tmp as usize];

    tmp = block[3];
    block[3] = t[0xf00 + block[7] as usize];
    block[7] = t[0x300 + block[11] as usize];
    block[11] = t[0x700 + block[15] as usize];
    block[15] = t[0xb00 + tmp as usize];
}

fn permute_block_2(block: &mut [u8], round: usize) {
    let t = &tables::TABLE_S4;
    let p = |i: usize| s4_base(round * 16 + i);
    block[0] = t[p(0) + block[0] as usize];
    block[4] = t[p(4) + block[4] as usize];
    block[8] = t[p(8) + block[8] as usize];
    block[12] = t[p(12) + block[12] as usize];

    let mut tmp = block[13];
    block[13] = t[p(13) + block[9] as usize];
    block[9] = t[p(9) + block[5] as usize];
    block[5] = t[p(5) + block[1] as usize];
    block[1] = t[p(1) + tmp as usize];

    tmp = block[2];
    block[2] = t[p(2) + block[10] as usize];
    block[10] = t[p(10) + tmp as usize];
    tmp = block[6];
    block[6] = t[p(6) + block[14] as usize];
    block[14] = t[p(14) + tmp as usize];

    tmp = block[3];
    block[3] = t[p(3) + block[7] as usize];
    block[7] = t[p(7) + block[11] as usize];
    block[11] = t[p(11) + block[15] as usize];
    block[15] = t[p(15) + tmp as usize];
}

fn generate_key_schedule(key_material: &[u8], key_schedule: &mut [[u32; 4]; 11]) {
    let mut buffer = [0u8; 16];
    for i in 0..16 {
        buffer[i] = key_material[i] ^ tables::T_KEY[i];
    }
    let mut ti = 0usize;
    for round in 0..11usize {
        key_schedule[round][0] = rd_le(&buffer, 0);

        let (t1, t2, t3, t4) = (
            s1_base(ti),
            s1_base(ti + 1),
            s1_base(ti + 2),
            s1_base(ti + 3),
        );
        ti += 4;
        buffer[0] ^= tables::TABLE_S1[t1 + buffer[0x0d] as usize] ^ tables::INDEX_MANGLE[round];
        buffer[1] ^= tables::TABLE_S1[t2 + buffer[0x0e] as usize];
        buffer[2] ^= tables::TABLE_S1[t3 + buffer[0x0f] as usize];
        buffer[3] ^= tables::TABLE_S1[t4 + buffer[0x0c] as usize];

        key_schedule[round][1] = rd_le(&buffer, 1);
        let w = rd_le(&buffer, 1) ^ rd_le(&buffer, 0);
        wr_le(&mut buffer, 1, w);

        key_schedule[round][2] = rd_le(&buffer, 2);
        let w = rd_le(&buffer, 2) ^ rd_le(&buffer, 1);
        wr_le(&mut buffer, 2, w);

        key_schedule[round][3] = rd_le(&buffer, 3);
        let w = rd_le(&buffer, 3) ^ rd_le(&buffer, 2);
        wr_le(&mut buffer, 3, w);
    }
}

fn cycle(block: &mut [u8], key_schedule: &[[u32; 4]; 11]) {
    for i in 0..4 {
        wr_le(block, i, rd_le(block, i) ^ key_schedule[10][i]);
    }
    permute_block_1(block);

    for round in 0..9usize {
        let ks = &key_schedule[9 - round];
        let kb = |w: usize, byte: usize| ((ks[w] >> (8 * byte)) & 0xff) as u8;

        let p1 = tables::TABLE_S5[(block[3] ^ kb(0, 3)) as usize];
        let p2 = tables::TABLE_S6[(block[2] ^ kb(0, 2)) as usize];
        let p3 = tables::TABLE_S8[(block[0] ^ kb(0, 0)) as usize];
        let p4 = tables::TABLE_S7[(block[1] ^ kb(0, 1)) as usize];
        wr_le(block, 0, p1 ^ p2 ^ p3 ^ p4);

        let p2 = tables::TABLE_S5[(block[7] ^ kb(1, 3)) as usize];
        let p1 = tables::TABLE_S6[(block[6] ^ kb(1, 2)) as usize];
        let p4 = tables::TABLE_S7[(block[5] ^ kb(1, 1)) as usize];
        let p3 = tables::TABLE_S8[(block[4] ^ kb(1, 0)) as usize];
        wr_le(block, 1, p1 ^ p2 ^ p3 ^ p4);

        let w2 = tables::TABLE_S5[(block[11] ^ kb(2, 3)) as usize]
            ^ tables::TABLE_S6[(block[10] ^ kb(2, 2)) as usize]
            ^ tables::TABLE_S7[(block[9] ^ kb(2, 1)) as usize]
            ^ tables::TABLE_S8[(block[8] ^ kb(2, 0)) as usize];
        wr_le(block, 2, w2);

        let w3 = tables::TABLE_S5[(block[15] ^ kb(3, 3)) as usize]
            ^ tables::TABLE_S6[(block[14] ^ kb(3, 2)) as usize]
            ^ tables::TABLE_S7[(block[13] ^ kb(3, 1)) as usize]
            ^ tables::TABLE_S8[(block[12] ^ kb(3, 0)) as usize];
        wr_le(block, 3, w3);

        permute_block_2(block, 8 - round);
    }

    for i in 0..4 {
        wr_le(block, i, rd_le(block, i) ^ key_schedule[0][i]);
    }
}

fn xor_blocks(a: &[u8], b: &[u8], out: &mut [u8]) {
    for i in 0..16 {
        out[i] = a[i] ^ b[i];
    }
}

fn decrypt_message(message_in: &[u8], decrypted_message: &mut [u8]) {
    let mode = message_in[12] as usize;

    for i in 0..8usize {
        let mut buffer = [0u8; 16];
        for j in 0..16usize {
            buffer[j] = if mode == 3 {
                message_in[(0x80 - 0x10 * i) + j]
            } else {
                message_in[(0x10 * (i + 1)) + j]
            };
        }

        for j in 0..9usize {
            let base = 0x80 - 0x10 * j;
            let mk = |col: usize| tables::MESSAGE_KEY[mode * 144 + base + col];
            let mt = |col: usize, val: u8| {
                tables::TABLE_S2[s2_base(base + col) + val as usize] ^ mk(col)
            };

            buffer[0x0] = mt(0x0, buffer[0x0]);
            buffer[0x4] = mt(0x4, buffer[0x4]);
            buffer[0x8] = mt(0x8, buffer[0x8]);
            buffer[0xc] = mt(0xc, buffer[0xc]);

            let mut tmp = buffer[0x0d];
            buffer[0xd] = mt(0xd, buffer[0x9]);
            buffer[0x9] = mt(0x9, buffer[0x5]);
            buffer[0x5] = mt(0x5, buffer[0x1]);
            buffer[0x1] = mt(0x1, tmp);

            tmp = buffer[0x02];
            buffer[0x2] = mt(0x2, buffer[0xa]);
            buffer[0xa] = mt(0xa, tmp);
            tmp = buffer[0x06];
            buffer[0x6] = mt(0x6, buffer[0xe]);
            buffer[0xe] = mt(0xe, tmp);

            tmp = buffer[0x3];
            buffer[0x3] = mt(0x3, buffer[0x7]);
            buffer[0x7] = mt(0x7, buffer[0xb]);
            buffer[0xb] = mt(0xb, buffer[0xf]);
            buffer[0xf] = mt(0xf, tmp);

            let s9 = &tables::TABLE_S9;
            let w0 = s9[0x000 + buffer[0x0] as usize]
                ^ s9[0x100 + buffer[0x1] as usize]
                ^ s9[0x200 + buffer[0x2] as usize]
                ^ s9[0x300 + buffer[0x3] as usize];
            let w1 = s9[0x000 + buffer[0x4] as usize]
                ^ s9[0x100 + buffer[0x5] as usize]
                ^ s9[0x200 + buffer[0x6] as usize]
                ^ s9[0x300 + buffer[0x7] as usize];
            let w2 = s9[0x000 + buffer[0x8] as usize]
                ^ s9[0x100 + buffer[0x9] as usize]
                ^ s9[0x200 + buffer[0xa] as usize]
                ^ s9[0x300 + buffer[0xb] as usize];
            let w3 = s9[0x000 + buffer[0xc] as usize]
                ^ s9[0x100 + buffer[0xd] as usize]
                ^ s9[0x200 + buffer[0xe] as usize]
                ^ s9[0x300 + buffer[0xf] as usize];
            wr_le(&mut buffer, 0, w0);
            wr_le(&mut buffer, 1, w1);
            wr_le(&mut buffer, 2, w2);
            wr_le(&mut buffer, 3, w3);
        }

        // table_s10 permute
        let t10 = |col: usize, val: u8| tables::TABLE_S10[(col << 8) + val as usize];
        buffer[0x0] = t10(0x0, buffer[0x0]);
        buffer[0x4] = t10(0x4, buffer[0x4]);
        buffer[0x8] = t10(0x8, buffer[0x8]);
        buffer[0xc] = t10(0xc, buffer[0xc]);

        let mut tmp = buffer[0x0d];
        buffer[0xd] = t10(0xd, buffer[0x9]);
        buffer[0x9] = t10(0x9, buffer[0x5]);
        buffer[0x5] = t10(0x5, buffer[0x1]);
        buffer[0x1] = t10(0x1, tmp);

        tmp = buffer[0x02];
        buffer[0x2] = t10(0x2, buffer[0xa]);
        buffer[0xa] = t10(0xa, tmp);
        tmp = buffer[0x06];
        buffer[0x6] = t10(0x6, buffer[0xe]);
        buffer[0xe] = t10(0xe, tmp);

        tmp = buffer[0x3];
        buffer[0x3] = t10(0x3, buffer[0x7]);
        buffer[0x7] = t10(0x7, buffer[0xb]);
        buffer[0xb] = t10(0xb, buffer[0xf]);
        buffer[0xf] = t10(0xf, tmp);

        if mode == 2 || mode == 1 || mode == 0 {
            if i > 0 {
                let mi = &message_in[0x10 * i..0x10 * i + 16];
                let mut out = [0u8; 16];
                xor_blocks(&buffer, mi, &mut out);
                decrypted_message[0x10 * i..0x10 * i + 16].copy_from_slice(&out);
            } else {
                let iv = &tables::MESSAGE_IV[mode * 16..mode * 16 + 16];
                let mut out = [0u8; 16];
                xor_blocks(&buffer, iv, &mut out);
                decrypted_message[0..16].copy_from_slice(&out);
            }
        } else if i < 7 {
            let off = 0x70 - 0x10 * i;
            let mi = &message_in[off..off + 16];
            let mut out = [0u8; 16];
            xor_blocks(&buffer, mi, &mut out);
            decrypted_message[off..off + 16].copy_from_slice(&out);
        } else {
            let off = 0x70 - 0x10 * i;
            let iv = &tables::MESSAGE_IV[mode * 16..mode * 16 + 16];
            let mut out = [0u8; 16];
            xor_blocks(&buffer, iv, &mut out);
            decrypted_message[off..off + 16].copy_from_slice(&out);
        }
    }
}

fn generate_session_key(old_sap: &[u8], message_in: &[u8], session_key: &mut [u8]) {
    let mut decrypted_message = [0u8; 128];
    decrypt_message(message_in, &mut decrypted_message);

    let mut new_sap = [0u8; 320];
    new_sap[0x000..0x000 + 0x11].copy_from_slice(&tables::STATIC_SOURCE_1[..0x11]);
    new_sap[0x011..0x011 + 0x80].copy_from_slice(&decrypted_message[..0x80]);
    new_sap[0x091..0x091 + 0x80].copy_from_slice(&old_sap[0x80..0x80 + 0x80]);
    new_sap[0x111..0x111 + 0x2f].copy_from_slice(&tables::STATIC_SOURCE_2[..0x2f]);

    session_key[..16].copy_from_slice(&tables::INITIAL_SESSION_KEY[..16]);

    let mut md5 = [0u8; 16];
    for round in 0..5usize {
        let base = &new_sap[round * 64..round * 64 + 64];
        md5::modified_md5(base, session_key, &mut md5);
        garble::sap_hash(base, session_key);
        for i in 0..4usize {
            let sk = rd_le(session_key, i).wrapping_add(rd_le(&md5, i));
            wr_le(session_key, i, sk);
        }
    }

    // Reverse each 4-byte group.
    for i in (0..16).step_by(4) {
        session_key.swap(i, i + 3);
        session_key.swap(i + 1, i + 2);
    }
    for i in 0..16 {
        session_key[i] ^= 121;
    }
}

fn playfair_decrypt(message3: &[u8], cipher_text: &[u8], key_out: &mut [u8]) {
    let chunk1 = &cipher_text[16..]; // 16 bytes used
    let chunk2 = &cipher_text[56..]; // 16 bytes used

    let mut sap_key = [0u8; 16];
    generate_session_key(tables::DEFAULT_SAP, message3, &mut sap_key);

    let mut key_schedule = [[0u32; 4]; 11];
    generate_key_schedule(&sap_key, &mut key_schedule);

    let mut block_in = [0u8; 16];
    z_xor(chunk2, &mut block_in, 1);
    cycle(&mut block_in, &key_schedule);

    for i in 0..16 {
        key_out[i] = block_in[i] ^ chunk1[i];
    }
    let tmp = key_out[..16].to_vec();
    x_xor(&tmp, key_out, 1);
    let tmp = key_out[..16].to_vec();
    z_xor(&tmp, key_out, 1);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    fn make_msg(seed: i32, mode: u8) -> [u8; 164] {
        let mut m = [0u8; 164];
        for i in 0..164 {
            m[i] = ((i as i32 * 31 + seed * 101 + 7) & 0xff) as u8;
        }
        m[4] = 0x03;
        m[12] = mode;
        m
    }

    fn make_cipher(seed: i32) -> [u8; 72] {
        let mut c = [0u8; 72];
        for i in 0..72 {
            c[i] = ((i as i32 * 53 + seed * 17 + 11) & 0xff) as u8;
        }
        c
    }

    #[test]
    fn md5_matches_reference() {
        let mut blk = [0u8; 64];
        for i in 0..64 {
            blk[i] = (i + 1) as u8;
        }
        let mut key = [0u8; 16];
        for i in 0..16 {
            key[i] = (0x10 + i) as u8;
        }
        let mut out = [0u8; 16];
        md5::modified_md5(&blk, &key, &mut out);
        assert_eq!(out.to_vec(), hex("ad3e2c46764e7e0cd303edb49c56c8b3"));
    }

    #[test]
    fn sap_hash_matches_reference() {
        let mut blk = [0u8; 64];
        for i in 0..64 {
            blk[i] = (i + 1) as u8;
        }
        let mut out = [0u8; 16];
        garble::sap_hash(&blk, &mut out);
        assert_eq!(out.to_vec(), hex("473370420e42c23179b93edcf224d744"));
    }

    #[test]
    fn decrypt_message_matches_reference() {
        let msg = make_msg(1, 1);
        let mut out = [0u8; 128];
        decrypt_message(&msg, &mut out);
        assert_eq!(
            out.to_vec(),
            hex(
                "0b0fcab68d57b307deb331dcd9411532c217b4b2fbabc426b17e736f9d06ca55\
                7ae401af749d485b1333490bc5127e27dbc6bdec3be9848e2d8d48e5feab6af3d\
                3a50a148edb31bde9c2596256ef0aa2a9700ee274cd065834bd0abaff1cc0ff77\
                91221a0e08d89cfe76c8bf85ea91317483aafc9dcd0d716ae4e15686d9770c"
            )
        );
    }

    #[test]
    fn session_key_matches_reference() {
        let msg = make_msg(1, 1);
        let mut sk = [0u8; 16];
        generate_session_key(tables::DEFAULT_SAP, &msg, &mut sk);
        assert_eq!(sk.to_vec(), hex("c61edf59c37fdb540dd727cde798b406"));
    }

    #[test]
    fn key_schedule_matches_reference() {
        let sap_key = hex("c61edf59c37fdb540dd727cde798b406");
        let mut ks = [[0u32; 4]; 11];
        generate_key_schedule(&sap_key, &mut ks);
        let mut bytes = Vec::new();
        for row in &ks {
            for w in row {
                bytes.extend_from_slice(&w.to_le_bytes());
            }
        }
        assert_eq!(
            bytes,
            hex(
                "161a7638a8dbdbd3655c78d8f2ad6dafa0050c2108ded7f26d82af2a9f2fc285\
                277421cd2faaf63f42285915dd079b90d94203c4f6e8f5fbb4c0acee69c7377e\
                807ec80976963df2c256911cab91a66286275e4ff0b163bd32e7f2a1997654c3\
                bc65ec2a4cd48f977e337d36e74529f509059d4145d112d63be26fe0dca74615\
                b07a1ef5f5ab0c23ce4963c312ee25d668e370819d487ca253011f6141ef3ab7\
                0becf21196a48eb3c5a591d2844aab65"
            )
        );
    }

    #[test]
    fn playfair_decrypt_stage_vector() {
        let msg = make_msg(1, 1);
        let cipher = make_cipher(1);
        let mut out = [0u8; 16];
        playfair_decrypt(&msg, &cipher, &mut out);
        assert_eq!(out.to_vec(), hex("096b445f933cddddc50fc3e40bd9ab6d"));
    }

    #[test]
    fn playfair_decrypt_all_modes() {
        let expected = [
            (0, 1, "4c53b8875e173a972b94ccdeb3622b1b"),
            (0, 2, "4ce6d7ea808afbc417ce519cc6da7d6c"),
            (1, 1, "096b445f933cddddc50fc3e40bd9ab6d"),
            (1, 2, "a754865c5752dc0697806395e09f9dcb"),
            (2, 1, "a7d0746ba4bfbc5f063b99dfcbc3a749"),
            (2, 2, "eeba32fe2779c9f921c0cc70b88841f0"),
            (3, 1, "f64ac89e4ff22d7109ef7fba4c741a7d"),
            (3, 2, "572cdef8463f0134085ae0381d274d94"),
        ];
        for (mode, seed, want) in expected {
            let msg = make_msg(seed, mode);
            let cipher = make_cipher(seed);
            let mut out = [0u8; 16];
            playfair_decrypt(&msg, &cipher, &mut out);
            assert_eq!(out.to_vec(), hex(want), "mode={mode} seed={seed}");
        }
    }

    #[test]
    fn fp_setup_reply_selects_mode() {
        let mut fp = FairPlay::new();
        let mut req = [0u8; 16];
        req[4] = 0x03;
        req[14] = 2;
        let res = fp.setup(&req).unwrap();
        assert_eq!(&res[..], &tables::REPLY_MESSAGE[2 * 142..3 * 142]);
    }

    #[test]
    fn fp_handshake_echoes_tail() {
        let mut fp = FairPlay::new();
        let mut req = [0u8; 164];
        req[4] = 0x03;
        for i in 0..164 {
            req[i] = req[i].wrapping_add((i * 3) as u8);
        }
        req[4] = 0x03;
        let res = fp.handshake(&req).unwrap();
        assert_eq!(&res[..12], tables::FP_HEADER);
        assert_eq!(&res[12..32], &req[144..164]);
        assert!(fp.have_keymsg);
    }
}
