//! Port of [PlayFair](https://github.com/EstebanKubata/playfair) `sap_hash.c` and `hand_garble.c`.
//!
//! `garble` is a deobfuscated block of FairPlay byte arithmetic. Every C operand here is `unsigned
//! char` (promoted to `int`) or an `unsigned int` temporary, so the faithful model is: read bytes
//! zero-extended to `u32`, do all arithmetic with 32-bit wrapping, and truncate to `u8` on
//! store. Validated byte-exact against the C reference by the `STAGE_saphash` test. Do not "clean
//! up" - the exact operations and precedence are load-bearing.

#![allow(clippy::needless_range_loop)]

fn rol8(input: u32, count: u32) -> u32 {
    (((input << count) & 0xff) | ((input & 0xff) >> (8 - count))) & 0xff
}

fn rol8x(input: u32, count: u32) -> u32 {
    (input << count) | (input >> (8 - count))
}

fn weird_ror8(input: u32, count: u32) -> u32 {
    if count == 0 {
        return 0;
    }
    ((input >> count) & 0xff) | ((input & 0xff) << (8 - count))
}

fn weird_rol8(input: u32, count: u32) -> u32 {
    if count == 0 {
        return 0;
    }
    ((input << count) & 0xff) | ((input & 0xff) >> (8 - count))
}

fn weird_rol32(input: u32, count: u32) -> u32 {
    if count == 0 {
        return 0;
    }
    (input << count) ^ (input >> (8 - count))
}

/// Index helper: `buffer[byte % modulus]`.
macro_rules! at {
    ($buf:expr, $idx:expr, $m:expr) => {
        $buf[($idx as usize) % $m]
    };
}

#[rustfmt::skip]
pub fn garble(b0: &mut [u8], b1: &mut [u8], b2: &mut [u8], b3: &mut [u8], b4: &mut [u8]) {
    // Sizes: b0=20, b1=210, b2=35, b3=132, b4=21.
    let (mut a, mut b, mut c, mut d, mut e, mut m, mut jj, mut g, mut ff, mut h, mut k,
         mut r, mut s, mut t, mut u, mut v, mut w, mut x, mut y, mut z): (u32,u32,u32,u32,u32,u32,u32,u32,u32,u32,u32,u32,u32,u32,u32,u32,u32,u32,u32,u32)
        = (0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0);
    let (tmp, tmp2, tmp3);

    // 41
    let i0 = rol8x(at!(b4, b1[206], 21) as u32, 4) as usize % 21;
    b2[12] = (0x14u32.wrapping_add(((b1[64] as u32 & 92) | ((b1[99] as u32 / 3) & 35)) & b4[i0] as u32)) as u8;
    // 45
    b1[4] = ((b1[99] as u32 / 5).wrapping_mul(b1[99] as u32 / 5).wrapping_mul(2)) as u8;
    // 49
    b2[34] = 0xb8;
    // 53
    b1[153] = (b1[153] as u32 ^ (at!(b2, b1[203], 35) as u32).wrapping_mul(at!(b2, b1[203], 35) as u32).wrapping_mul(b1[190] as u32)) as u8;
    // 57
    b0[3] = (b0[3] as u32).wrapping_sub((((at!(b4, b1[205], 21) as u32) >> 1) & 80) | 0xe6440) as u8;
    // 61
    b0[16] = 0x93;
    // 65
    b0[13] = 0x62;
    // 68
    b1[33] = (b1[33] as u32).wrapping_sub(at!(b4, b1[36], 21) as u32 & 0xf6) as u8;
    // 72
    tmp2 = at!(b2, b1[67], 35) as u32;
    b2[12] = 0x07;
    // 77
    tmp = at!(b0, b1[181], 20) as u32;
    b1[2] = (b1[2] as u32).wrapping_sub(3136) as u8;
    // 81
    b0[19] = at!(b4, b1[58], 21);
    // 84
    b3[0] = (92u32.wrapping_sub(at!(b2, b1[32], 35) as u32)) as u8;
    // 87
    b3[4] = (at!(b2, b1[15], 35) as u32).wrapping_add(0x9e) as u8;
    // 90
    b1[34] = (b1[34] as u32).wrapping_add(at!(b4, ((at!(b2, b1[15], 35) as u32).wrapping_add(0x9e) & 0xff), 21) as u32 / 5) as u8;
    // 93
    b0[19] = (b0[19] as u32).wrapping_add(0xfffffee6u32.wrapping_sub(((at!(b0, b3[4], 20) as u32) >> 1) & 102)) as u8;
    // 98
    {
        let i190 = at!(b4, b1[190], 21) as u32;
        let sa = i190 & 7;
        let sb = (7u32.wrapping_sub(i190.wrapping_sub(1))) & 7;
        let t72 = ((b1[72] as u32) >> sa) ^ ((b1[72] as u32) << sb);
        let mm = 3u32.wrapping_mul(t72.wrapping_sub(3u32.wrapping_mul(at!(b4, b1[126], 21) as u32)));
        b1[15] = (mm ^ b1[15] as u32) as u8;
    }
    // 101
    b0[15] = (b0[15] as u32 ^ (at!(b2, b1[181], 35) as u32).wrapping_mul(at!(b2, b1[181], 35) as u32).wrapping_mul(at!(b2, b1[181], 35) as u32)) as u8;
    // 104
    b2[4] = (b2[4] as u32 ^ (b1[202] as u32 / 3)) as u8;
    // 108
    a = 92u32.wrapping_sub(at!(b0, b3[0], 20) as u32);
    e = (a & 0xc6) | (!(b1[105] as u32) & 0xc6) | (a & !(b1[105] as u32));
    b2[1] = (b2[1] as u32).wrapping_add(e.wrapping_mul(e).wrapping_mul(e)) as u8;
    // 113
    b0[19] = (b0[19] as u32 ^ (((224 | (at!(b4, b1[92], 21) as u32 & 27)).wrapping_mul(at!(b2, b1[41], 35) as u32)) / 3)) as u8;
    // 116
    b1[140] = (b1[140] as u32).wrapping_add(weird_ror8(92, b1[5] as u32 & 7)) as u8;
    // 120
    b2[12] = (b2[12] as u32).wrapping_add(
        ((((!(b1[4] as u32)) ^ at!(b2, b1[12], 35) as u32) | b1[182] as u32) & 192)
        | (((!(b1[4] as u32)) ^ at!(b2, b1[12], 35) as u32) & b1[182] as u32)) as u8;
    // 123
    b1[36] = (b1[36] as u32).wrapping_add(125) as u8;
    // 126
    b1[124] = rol8x(
        ((((74 & b1[138] as u32) | ((74 | b1[138] as u32) & b0[15] as u32)) & at!(b0, b1[43], 20) as u32)
        | (((74 & b1[138] as u32) | ((74 | b1[138] as u32) & b0[15] as u32) | at!(b0, b1[43], 20) as u32) & 95)), 4) as u8;
    // 129
    b3[8] = (((((at!(b0, b3[4], 20) as u32 & 95)) & ((at!(b4, b1[68], 21) as u32 & 46) << 1)) | 16) ^ 92) as u8;
    // 132
    a = (b1[177] as u32).wrapping_add(at!(b4, b1[79], 21) as u32);
    d = (((a >> 1) | ((3u32.wrapping_mul(b1[148] as u32)) / 5)) & b2[1] as u32) | ((a >> 1) & ((3u32.wrapping_mul(b1[148] as u32)) / 5));
    b3[12] = ((0u32.wrapping_sub(34)).wrapping_sub(d)) as u8;
    // 137
    a = 8u32.wrapping_sub(b2[22] as u32 & 7);
    b = (b1[33] as u32) >> (a & 7);
    c = (b1[33] as u32) << (b2[22] as u32 & 7);
    b2[16] = (b2[16] as u32).wrapping_add(
        ((at!(b2, b3[0], 35) as u32 & 159) | at!(b0, b3[4], 20) as u32 | 8).wrapping_sub((b ^ c) | 128)) as u8;
    // 144
    b0[14] = (b0[14] as u32 ^ at!(b2, b3[12], 35) as u32) as u8;
    // 148
    a = weird_rol8(at!(b4, at!(b0, b1[201], 20), 21) as u32, ((at!(b2, b1[112], 35) as u32) << 1) & 7);
    d = (at!(b0, b1[208], 20) as u32 & 131) | (at!(b0, b1[164], 20) as u32 & 124);
    b1[19] = (b1[19] as u32).wrapping_add((a & (d / 5)) | ((a | (d / 5)) & 37)) as u8;
    // 153
    b2[8] = weird_ror8(140, ((at!(b4, b1[45], 21) as u32).wrapping_add(92)).wrapping_mul((at!(b4, b1[45], 21) as u32).wrapping_add(92)) & 7) as u8;
    // 156
    b1[190] = 56;
    // 159
    b2[8] = (b2[8] as u32 ^ b3[0] as u32) as u8;
    // 162
    b1[53] = (!((at!(b0, b1[83], 20) as u32 | 204) / 5)) as u8;
    // 165
    b0[13] = (b0[13] as u32).wrapping_add(at!(b0, b1[41], 20) as u32) as u8;
    // 168
    b0[10] = (((at!(b2, b3[0], 35) as u32 & b1[2] as u32) | ((at!(b2, b3[0], 35) as u32 | b1[2] as u32) & b3[12] as u32)) / 15) as u8;
    // 171
    a = (((56 | (at!(b4, b1[2], 21) as u32 & 68)) | at!(b2, b3[8], 35) as u32) & 42) | (((at!(b4, b1[2], 21) as u32 & 68) | 56) & at!(b2, b3[8], 35) as u32);
    b3[16] = (a.wrapping_mul(a).wrapping_add(110)) as u8;
    // 175
    b3[20] = (202u32.wrapping_sub(b3[16] as u32)) as u8;
    // 178
    b3[24] = b1[151];
    // 181
    b2[13] = (b2[13] as u32 ^ at!(b4, b3[0], 21) as u32) as u8;
    // 184
    b = ((at!(b2, b1[179], 35) as u32).wrapping_sub(38) & 177) | (b3[12] as u32 & 177);
    c = (at!(b2, b1[179], 35) as u32).wrapping_sub(38) & b3[12] as u32;
    b3[28] = (30u32.wrapping_add((b | c).wrapping_mul(b | c))) as u8;
    // 189
    b3[32] = (b3[28] as u32).wrapping_add(62) as u8;
    // 193
    a = ((b3[20] as u32).wrapping_add(b3[0] as u32 & 74) | !(at!(b4, b3[0], 21) as u32)) & 121;
    b = (b3[20] as u32).wrapping_add(b3[0] as u32 & 74) & !(at!(b4, b3[0], 21) as u32);
    tmp3 = a | b;
    c = ((((a | b) ^ 0xffffffa6) | b3[0] as u32) & 4) | (((a | b) ^ 0xffffffa6) & b3[0] as u32);
    b1[47] = ((at!(b2, b1[89], 35) as u32).wrapping_add(c) ^ b1[47] as u32) as u8;
    // 200
    b3[36] = ((rol8((tmp & 179).wrapping_add(68), 2) & b0[3] as u32) | (tmp2 & !(b0[3] as u32))).wrapping_sub(15) as u8;
    // 203
    b1[123] = (b1[123] as u32 ^ 221) as u8;
    // 206
    a = (at!(b4, b3[0], 21) as u32 / 3).wrapping_sub(at!(b2, b3[4], 35) as u32);
    c = (((b3[0] as u32 & 163).wrapping_add(92)) & 246) | (b3[0] as u32 & 92);
    e = ((c | b3[24] as u32) & 54) | (c & b3[24] as u32);
    b3[40] = (a.wrapping_sub(e)) as u8;
    // 212
    b3[44] = (tmp3 ^ 81 ^ ((((b3[0] as u32) >> 1) & 101).wrapping_add(26))) as u8;
    // 215
    b3[48] = (at!(b2, b3[4], 35) as u32 & 27) as u8;
    b3[52] = 27;
    b3[56] = 199;
    // 223
    b3[64] = (b3[4] as u32).wrapping_add(
        (((((((b3[40] as u32 | b3[24] as u32) & 177) | (b3[40] as u32 & b3[24] as u32)) & ((((at!(b4, b3[0], 20) as u32 & 177) | 176)) | ((at!(b4, b3[0], 21) as u32) & !3)))
        | ((((b3[40] as u32 & b3[24] as u32) | ((b3[40] as u32 | b3[24] as u32) & 177)) & 199) | ((((at!(b4, b3[0], 21) as u32 & 1).wrapping_add(176)) | (at!(b4, b3[0], 21) as u32 & !3)) & b3[56] as u32))) & (!(b3[52] as u32)))
        | b3[48] as u32)) as u8;
    // 226
    b2[33] = (b2[33] as u32 ^ b1[26] as u32) as u8;
    // 229
    b1[106] = (b1[106] as u32 ^ b3[20] as u32 ^ 133) as u8;
    // 232
    b2[30] = (((b3[64] as u32 / 3).wrapping_sub(275 | (b3[0] as u32 & 247))) ^ at!(b0, b1[122], 20) as u32) as u8;
    // 235
    b1[22] = ((at!(b2, b1[90], 35) as u32 & 95) | 68) as u8;
    // 238
    a = (at!(b4, b3[36], 21) as u32 & 184) | (at!(b2, b3[44], 35) as u32 & !184);
    b2[18] = (b2[18] as u32).wrapping_add((a.wrapping_mul(a).wrapping_mul(a)) >> 1) as u8;
    // 242
    b2[5] = (b2[5] as u32).wrapping_sub(at!(b4, b1[92], 21) as u32) as u8;
    // 245
    a = (((b1[41] as u32 & !24) | (at!(b2, b1[183], 35) as u32 & 24)) & (b3[16] as u32).wrapping_add(53)) | (b3[20] as u32 & at!(b2, b3[20], 35) as u32);
    b = (b1[17] as u32 & (!(b3[44] as u32))) | (at!(b0, b1[59], 20) as u32 & b3[44] as u32);
    b2[18] = (b2[18] as u32 ^ a.wrapping_mul(b)) as u8;
    // 251
    a = weird_ror8(b1[11] as u32, at!(b2, b1[28], 35) as u32 & 7) & 7;
    b = (((at!(b0, b1[93], 20) as u32 & !(b0[14] as u32)) | (b0[14] as u32 & 150)) & !28) | (b1[7] as u32 & 28);
    b2[22] = ((((b | weird_rol8(at!(b2, b3[0], 35) as u32, a)) & b2[33] as u32) | (b & weird_rol8(at!(b2, b3[0], 35) as u32, a))).wrapping_add(74) & 0xff) as u8;
    // 256
    a = at!(b4, (at!(b0, b1[39], 20) as u32 ^ 217), 21) as u32;
    b0[15] = (b0[15] as u32).wrapping_sub(
        (((b3[20] as u32 | b3[0] as u32) & 214) | (b3[20] as u32 & b3[0] as u32)) & a
        | ((((b3[20] as u32 | b3[0] as u32) & 214) | (b3[20] as u32 & b3[0] as u32) | a) & b3[32] as u32)) as u8;
    // 261
    b = (b2[ (b1[57] as usize) %35] as u32 & at!(b0, b3[64], 20) as u32) | ((at!(b0, b3[64], 20) as u32 | at!(b2, b1[57], 35) as u32) & 95) | (b3[64] as u32 & 45) | 82;
    b &= 32;
    c = ((at!(b2, b1[57], 35) as u32 & at!(b0, b3[64], 20) as u32) | ((at!(b2, b1[57], 35) as u32 | at!(b0, b3[64], 20) as u32) & 95)) & ((b3[64] as u32 & 45) | 82);
    d = ((b3[0] as u32 / 3).wrapping_sub(b3[64] as u32 | b1[22] as u32)) ^ ((b3[28] as u32).wrapping_add(62)) ^ (b | c);
    t = at!(b0, (d & 0xff), 20) as u32;
    // 266
    b3[68] = ((at!(b0, b1[99], 20) as u32).wrapping_mul(at!(b0, b1[99], 20) as u32).wrapping_mul(at!(b0, b1[99], 20) as u32).wrapping_mul(at!(b0, b1[99], 20) as u32) | at!(b2, b3[64], 35) as u32) as u8;
    // 269
    u = at!(b0, b1[50], 20) as u32;
    w = at!(b2, b1[138], 35) as u32;
    x = at!(b4, b1[39], 21) as u32;
    y = at!(b0, b1[4], 20) as u32;
    z = at!(b4, b1[202], 21) as u32;
    v = at!(b0, b1[151], 20) as u32;
    s = at!(b2, b1[14], 35) as u32;
    r = at!(b0, b1[145], 20) as u32;
    // 278
    a = (at!(b2, b3[68], 35) as u32 & at!(b0, b1[209], 20) as u32) | ((at!(b2, b3[68], 35) as u32 | at!(b0, b1[209], 20) as u32) & 24);
    b = weird_rol8(at!(b4, b1[127], 21) as u32, at!(b2, b3[68], 35) as u32 & 7);
    c = (a & b0[10] as u32) | (b & !(b0[10] as u32));
    d = 7 ^ ((at!(b4, at!(b2, b3[36], 35), 21) as u32) << 1);
    b3[72] = ((c & 71) | (d & !71)) as u8;
    // 285
    b2[2] = (b2[2] as u32).wrapping_add(
        ((((at!(b0, b3[20], 20) as u32) << 1) & 159) | (at!(b4, b1[190], 21) as u32 & !159))
        & ((((at!(b4, b3[64], 21) as u32 & 110) | (at!(b0, b1[25], 20) as u32 & !110)) & !150) | (b1[25] as u32 & 150))) as u8;
    // 288
    b2[14] = (b2[14] as u32).wrapping_sub(
        ((at!(b2, b3[20], 35) as u32 & (b3[72] as u32 ^ at!(b2, b1[100], 35) as u32)) & !34) | (b1[97] as u32 & 34)) as u8;
    // 291
    b0[17] = 115;
    // 294
    b1[23] = (b1[23] as u32 ^ (
        (((((at!(b4, b1[17], 21) as u32 | at!(b0, b3[20], 20) as u32) & b3[72] as u32) | (at!(b4, b1[17], 21) as u32 & at!(b0, b3[20], 20) as u32)) & (b1[50] as u32 / 3))
        | ((((at!(b4, b1[17], 21) as u32 | at!(b0, b3[20], 20) as u32) & b3[72] as u32) | (at!(b4, b1[17], 21) as u32 & at!(b0, b3[20], 20) as u32) | (b1[50] as u32 / 3)) & 246)) << 1)) as u8;
    // 298
    b0[13] = (((((((at!(b0, b3[40], 20) as u32 | b1[10] as u32) & 82) | (at!(b0, b3[40], 20) as u32 & b1[10] as u32)) & 209)
        | (((at!(b0, b1[39], 20) as u32) << 1) & 46)) >> 1)) as u8;
    // 302
    b2[33] = (b2[33] as u32).wrapping_sub(b1[113] as u32 & 9) as u8;
    // 305
    b2[28] = (b2[28] as u32).wrapping_sub((((2 | (b1[110] as u32 & 222)) >> 1) & !223) | (b3[20] as u32 & 223)) as u8;
    // 308
    jj = weird_rol8(v | z, u & 7);
    a = (b2[16] as u32 & t) | (w & (!(b2[16] as u32)));
    b = (b1[33] as u32 & 17) | (x & !17);
    e = ((y | (a.wrapping_add(b) / 5)) & 147) | (y & (a.wrapping_add(b) / 5));
    m = (b3[40] as u32 & at!(b4, ((b3[8] as u32).wrapping_add(jj).wrapping_add(e) & 0xff), 21) as u32) | ((b3[40] as u32 | at!(b4, ((b3[8] as u32).wrapping_add(jj).wrapping_add(e) & 0xff), 21) as u32) & b2[23] as u32);
    // 316
    b0[15] = ((((at!(b4, b3[20], 21) as u32).wrapping_sub(48) & (!(b1[184] as u32))) | ((at!(b4, b3[20], 21) as u32).wrapping_sub(48) & 189) | (189 & !(b1[184] as u32))) & m.wrapping_mul(m).wrapping_mul(m)) as u8;
    // 319
    b2[22] = (b2[22] as u32).wrapping_add(b1[183] as u32) as u8;
    // 322
    b3[76] = ((3u32.wrapping_mul(at!(b4, b1[1], 21) as u32)) ^ b3[0] as u32) as u8;
    // 325
    a = at!(b2, ((b3[8] as u32).wrapping_add(jj.wrapping_add(e)) & 0xff), 35) as u32;
    ff = ((at!(b4, b1[178], 21) as u32 & a) | ((at!(b4, b1[178], 21) as u32 | a) & 209)).wrapping_mul(at!(b0, b1[13], 20) as u32).wrapping_mul((at!(b4, b1[26], 21) as u32) >> 1);
    g = (ff.wrapping_add(0x733ffff9)).wrapping_mul(198).wrapping_sub(((ff.wrapping_add(0x733ffff9)).wrapping_mul(396).wrapping_add(212)) & 212).wrapping_add(85);
    b3[80] = (b3[36] as u32).wrapping_add(g ^ 148).wrapping_add((g ^ 107) << 1).wrapping_sub(127) as u8;
    // 331
    b3[84] = ((at!(b2, b3[64], 35) as u32 & 245) | (at!(b2, b3[20], 35) as u32 & 10)) as u8;
    // 334
    a = at!(b0, b3[68], 20) as u32 | 81;
    b2[18] = (b2[18] as u32).wrapping_sub((a.wrapping_mul(a).wrapping_mul(a) & !(b0[15] as u32)) | ((b3[80] as u32 / 15) & b0[15] as u32)) as u8;
    // 338
    b3[88] = ((b3[8] as u32).wrapping_add(jj).wrapping_add(e).wrapping_sub(at!(b0, b1[160], 20) as u32).wrapping_add(at!(b4, at!(b0, ((b3[8] as u32).wrapping_add(jj).wrapping_add(e) & 255), 20), 21) as u32 / 3)) as u8;
    // 341
    b = ((r ^ b3[72] as u32) & !198) | ((s.wrapping_mul(s)) & 198);
    ff = (at!(b4, b1[69], 21) as u32 & b1[172] as u32) | ((at!(b4, b1[69], 21) as u32 | b1[172] as u32) & ((b3[12] as u32).wrapping_sub(b).wrapping_add(77)));
    b0[16] = (147u32.wrapping_sub((b3[72] as u32 & ((ff & 251) | 1)) | (((ff & 250) | b3[72] as u32) & 198))) as u8;
    // 346
    c = (at!(b4, b1[168], 21) as u32 & at!(b0, b1[29], 20) as u32 & 7) | ((at!(b4, b1[168], 21) as u32 | at!(b0, b1[29], 20) as u32) & 6);
    ff = (at!(b4, b1[155], 21) as u32 & b1[105] as u32) | ((at!(b4, b1[155], 21) as u32 | b1[105] as u32) & 141);
    b0[3] = (b0[3] as u32).wrapping_sub(at!(b4, (weird_rol32(ff, c) % 21), 21) as u32) as u8;
    // 351
    b1[5] = (weird_ror8(b0[12] as u32, (at!(b0, b1[61], 20) as u32 / 5) & 7) ^ (((!(at!(b2, b3[84], 35) as u32)) & 0xffffffff) / 5)) as u8;
    // 354
    b1[198] = (b1[198] as u32).wrapping_add(b1[3] as u32) as u8;
    // 357
    a = 162 | at!(b2, b3[64], 35) as u32;
    b1[164] = (b1[164] as u32).wrapping_add(a.wrapping_mul(a) / 5) as u8;
    // 361
    g = weird_ror8(139, b3[80] as u32 & 7);
    c = ((at!(b4, b3[64], 21) as u32).wrapping_mul(at!(b4, b3[64], 21) as u32).wrapping_mul(at!(b4, b3[64], 21) as u32) & 95) | (at!(b0, b3[40], 20) as u32 & !95);
    b3[92] = ((g & 12) | (at!(b0, b3[20], 20) as u32 & 12) | (g & at!(b0, b3[20], 20) as u32) | c) as u8;
    // 366
    b2[12] = (b2[12] as u32).wrapping_add(((b1[103] as u32 & 32) | (b3[92] as u32 & (b1[103] as u32 | 60)) | 16) / 3) as u8;
    // 369
    b3[96] = b1[143];
    b3[100] = 27;
    // 375
    b3[104] = (((b3[40] as u32 & !(b2[8] as u32)) | (b1[35] as u32 & b2[8] as u32)) & b3[64] as u32 ^ 119) as u8;
    // 378
    b3[108] = (238 & ((((b3[40] as u32 & !(b2[8] as u32)) | (b1[35] as u32 & b2[8] as u32)) & b3[64] as u32) << 1)) as u8;
    // 381
    b3[112] = ((!(b3[64] as u32) & (b3[84] as u32 / 3)) ^ 49) as u8;
    // 384
    b3[116] = (98 & ((!(b3[64] as u32) & (b3[84] as u32 / 3)) << 1)) as u8;
    // 388
    a = (b1[35] as u32 & b2[8] as u32) | (b3[40] as u32 & !(b2[8] as u32));
    b = (a & b3[64] as u32) | ((b3[84] as u32 / 3) & !(b3[64] as u32));
    b1[143] = (b3[96] as u32).wrapping_sub(
        (b & (86u32.wrapping_add((b1[172] as u32 & 64) >> 1)))
        | (((((b1[172] as u32 & 65) >> 1) ^ 86) | ((!(b3[64] as u32) & (b3[84] as u32 / 3)) | (((b3[40] as u32 & !(b2[8] as u32)) | (b1[35] as u32 & b2[8] as u32)) & b3[64] as u32))) & b3[100] as u32)) as u8;
    // 393
    b2[29] = 162;
    // 396
    a = (((at!(b4, b3[88], 21) as u32 & 160) | (at!(b0, b1[125], 20) as u32 & 95)) >> 1);
    b = at!(b2, b1[149], 35) as u32 ^ (b1[43] as u32).wrapping_mul(b1[43] as u32);
    b0[15] = (b0[15] as u32).wrapping_add((b & a) | ((a | b) & 115)) as u8;
    // 401
    b3[120] = ((b3[64] as u32).wrapping_sub(at!(b0, b3[40], 20) as u32)) as u8;
    // 404
    b1[95] = at!(b4, b3[20], 21);
    // 407
    a = weird_ror8(at!(b2, b3[80], 35) as u32, (at!(b2, b1[17], 35) as u32).wrapping_mul(at!(b2, b1[17], 35) as u32).wrapping_mul(at!(b2, b1[17], 35) as u32) & 7);
    b0[7] = (b0[7] as u32).wrapping_sub(a.wrapping_mul(a)) as u8;
    // 411
    b2[8] = ((b2[8] as u32).wrapping_sub(b1[184] as u32).wrapping_add((at!(b4, b1[202], 21) as u32).wrapping_mul(at!(b4, b1[202], 21) as u32).wrapping_mul(at!(b4, b1[202], 21) as u32))) as u8;
    // 414
    b0[16] = (((at!(b2, b1[102], 35) as u32) << 1) & 132) as u8;
    // 417
    b3[124] = (((at!(b4, b3[40], 21) as u32) >> 1) ^ b3[68] as u32) as u8;
    // 420
    b0[7] = (b0[7] as u32).wrapping_sub(
        (at!(b0, b1[191], 20) as u32).wrapping_sub((((at!(b4, b1[80], 21) as u32) << 1) & !177) | (at!(b4, at!(b4, b3[88], 21), 21) as u32 & 177))) as u8;
    // 423
    b0[6] = at!(b0, b1[119], 20);
    // 426
    a = (at!(b4, b1[190], 21) as u32 & !209) | (b1[118] as u32 & 209);
    b = (at!(b0, b3[120], 20) as u32).wrapping_mul(at!(b0, b3[120], 20) as u32);
    b0[12] = ((at!(b0, b3[84], 20) as u32 ^ ((at!(b2, b1[71], 35) as u32).wrapping_add(at!(b2, b1[15], 35) as u32))) & ((a & b) | ((a | b) & 27))) as u8;
    // 431
    b = (b1[32] as u32 & at!(b2, b3[88], 35) as u32) | ((b1[32] as u32 | at!(b2, b3[88], 35) as u32) & 23);
    d = ((at!(b4, b1[57], 21) as u32).wrapping_mul(231) & 169) | (b & 86);
    ff = (((at!(b0, b1[82], 20) as u32 & !29) | (at!(b4, b3[124], 21) as u32 & 29)) & 190) | (at!(b4, (d / 5), 21) as u32 & !190);
    h = (at!(b0, b3[40], 20) as u32).wrapping_mul(at!(b0, b3[40], 20) as u32).wrapping_mul(at!(b0, b3[40], 20) as u32);
    k = (h & b1[82] as u32) | (h & 92) | (b1[82] as u32 & 92);
    b3[128] = (((ff & k) | ((ff | k) & 192)) ^ (d / 5)) as u8;
    // 439
    b2[25] = (b2[25] as u32 ^ (
        (((at!(b0, b3[120], 20) as u32) << 1).wrapping_mul(b1[5] as u32))
        .wrapping_sub(weird_rol8(b3[76] as u32, at!(b4, b3[124], 21) as u32 & 7) & ((b3[20] as u32).wrapping_add(110))))) as u8;

    // Silence unused-assignment warnings for temporaries that mirror C scratch vars.
    let _ = (jj, m, ff, g, h, k, r, s, t, u, v, w, x, y, z, tmp, tmp2, tmp3, b, c, d, e, a);
}

/// Port of `sap_hash`. `block_in` is 64 bytes, `key_out` is 16 bytes (also an input seed).
pub fn sap_hash(block_in: &[u8], key_out: &mut [u8]) {
    let mut buffer0: [u8; 20] = [
        0x96, 0x5F, 0xC6, 0x53, 0xF8, 0x46, 0xCC, 0x18, 0xDF, 0xBE, 0xB2, 0xF8, 0x38, 0xD7, 0xEC,
        0x22, 0x03, 0xD1, 0x20, 0x8F,
    ];
    let mut buffer1 = [0u8; 210];
    let mut buffer2: [u8; 35] = [
        0x43, 0x54, 0x62, 0x7A, 0x18, 0xC3, 0xD6, 0xB3, 0x9A, 0x56, 0xF6, 0x1C, 0x14, 0x3F, 0x0C,
        0x1D, 0x3B, 0x36, 0x83, 0xB1, 0x39, 0x51, 0x4A, 0xAA, 0x09, 0x3E, 0xFE, 0x44, 0xAF, 0xDE,
        0xC3, 0x20, 0x9D, 0x42, 0x3A,
    ];
    let mut buffer3 = [0u8; 132];
    let mut buffer4: [u8; 21] = [
        0xED, 0x25, 0xD1, 0xBB, 0xBC, 0x27, 0x9F, 0x02, 0xA2, 0xA9, 0x11, 0x00, 0x0C, 0xB3, 0x52,
        0xC0, 0xBD, 0xE3, 0x1B, 0x49, 0xC7,
    ];
    let i0_index: [usize; 11] = [18, 22, 23, 0, 5, 19, 32, 31, 10, 21, 30];

    let block_words = |w: usize| -> u32 {
        let o = w * 4;
        u32::from_le_bytes([
            block_in[o],
            block_in[o + 1],
            block_in[o + 2],
            block_in[o + 3],
        ])
    };

    // Load the input (byte-swapped within each LE word).
    for i in 0..210usize {
        let in_word = block_words((i % 64) >> 2);
        let in_byte = (in_word >> ((3 - (i % 4)) << 3)) & 0xff;
        buffer1[i] = in_byte as u8;
    }

    // Scramble.
    for i in 0..840usize {
        let x = buffer1[((i as u32).wrapping_sub(155) % 210) as usize] as u32;
        let y = buffer1[((i as u32).wrapping_sub(57) % 210) as usize] as u32;
        let z = buffer1[((i as u32).wrapping_sub(13) % 210) as usize] as u32;
        let w = buffer1[(i % 210)] as u32;
        buffer1[i % 210] = (rol8(y, 5)
            .wrapping_add(rol8(z, 3) ^ w)
            .wrapping_sub(rol8(x, 7))
            & 0xff) as u8;
    }

    garble(
        &mut buffer0,
        &mut buffer1,
        &mut buffer2,
        &mut buffer3,
        &mut buffer4,
    );

    for i in 0..16usize {
        key_out[i] = 0xE1;
    }

    // buffer3 fold (index 3 hard-coded).
    for i in 0..11usize {
        if i == 3 {
            key_out[i] = 0x3d;
        } else {
            key_out[i] =
                ((key_out[i] as u32).wrapping_add(buffer3[i0_index[i] * 4] as u32) & 0xff) as u8;
        }
    }
    // buffer0
    for i in 0..20usize {
        key_out[i % 16] ^= buffer0[i];
    }
    // buffer2
    for i in 0..35usize {
        key_out[i % 16] ^= buffer2[i];
    }
    // buffer1
    for i in 0..210usize {
        key_out[i % 16] ^= buffer1[i];
    }

    // Reverse-scramble.
    for _j in 0..16usize {
        for i in 0..16usize {
            let x = key_out[((i as u32).wrapping_sub(7) % 16) as usize] as u32;
            let y = key_out[i % 16] as u32;
            let z = key_out[((i as u32).wrapping_sub(37) % 16) as usize] as u32;
            let w = key_out[((i as u32).wrapping_sub(177) % 16) as usize] as u32;
            key_out[i] = (rol8(x, 1) ^ y ^ rol8(z, 6) ^ rol8(w, 5)) as u8;
        }
    }
}
