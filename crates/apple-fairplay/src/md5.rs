//! Port of [PlayFair](https://github.com/EstebanKubata/playfair) `modified_md5.c`.
//!
//! A heavily-modified MD5 used by FairPlay's session-key derivation. The wrinkle
//! versus stock MD5 is the register rotation and the block-word swap at i==31.
//! All arithmetic is 32-bit wrapping, matching C `uint32_t` semantics.

/// Standard MD5 K constants: floor(2^32 * |sin(i+1)|). The C code computes these
/// at runtime via `(int)(long long)((1LL<<32)*fabs(sin(i+1)))`; these are the
/// well-known precomputed values. Validated byte-exact by the `STAGE_md5` test.
#[rustfmt::skip]
const K: [u32; 64] = [
    0xd76aa478, 0xe8c7b756, 0x242070db, 0xc1bdceee, 0xf57c0faf, 0x4787c62a, 0xa8304613, 0xfd469501,
    0x698098d8, 0x8b44f7af, 0xffff5bb1, 0x895cd7be, 0x6b901122, 0xfd987193, 0xa679438e, 0x49b40821,
    0xf61e2562, 0xc040b340, 0x265e5a51, 0xe9b6c7aa, 0xd62f105d, 0x02441453, 0xd8a1e681, 0xe7d3fbc8,
    0x21e1cde6, 0xc33707d6, 0xf4d50d87, 0x455a14ed, 0xa9e3e905, 0xfcefa3f8, 0x676f02d9, 0x8d2a4c8a,
    0xfffa3942, 0x8771f681, 0x6d9d6122, 0xfde5380c, 0xa4beea44, 0x4bdecfa9, 0xf6bb4b60, 0xbebfbc70,
    0x289b7ec6, 0xeaa127fa, 0xd4ef3085, 0x04881d05, 0xd9d4d039, 0xe6db99e5, 0x1fa27cf8, 0xc4ac5665,
    0xf4292244, 0x432aff97, 0xab9423a7, 0xfc93a039, 0x655b59c3, 0x8f0ccc92, 0xffeff47d, 0x85845dd1,
    0x6fa87e4f, 0xfe2ce6e0, 0xa3014314, 0x4e0811a1, 0xf7537e82, 0xbd3af235, 0x2ad7d2bb, 0xeb86d391,
];

#[rustfmt::skip]
const SHIFT: [u32; 64] = [
    7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22,
    5,  9, 14, 20, 5,  9, 14, 20, 5,  9, 14, 20, 5,  9, 14, 20,
    4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23,
    6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
];

// TODO: use more descriptive names for the functions and variables in this module

fn f(b: u32, c: u32, d: u32) -> u32 {
    (b & c) | (!b & d)
}
fn g(b: u32, c: u32, d: u32) -> u32 {
    (b & d) | (c & !d)
}
fn h(b: u32, c: u32, d: u32) -> u32 {
    b ^ c ^ d
}
fn i_fn(b: u32, c: u32, d: u32) -> u32 {
    c ^ (b | !d)
}

fn rol(input: u32, count: u32) -> u32 {
    input.rotate_left(count)
}

fn rd_le(buf: &[u8], word: usize) -> u32 {
    let o = word * 4;
    u32::from_le_bytes([buf[o], buf[o + 1], buf[o + 2], buf[o + 3]])
}
fn wr_le(buf: &mut [u8], word: usize, v: u32) {
    let o = word * 4;
    buf[o..o + 4].copy_from_slice(&v.to_le_bytes());
}

/// `originalblock_in` is 64 bytes, `key_in` and `key_out` are 16 bytes.
pub fn modified_md5(original_block_in: &[u8], key_in: &[u8], key_out: &mut [u8]) {
    let mut block_in = [0u8; 64];
    block_in.copy_from_slice(&original_block_in[..64]);

    let k0 = rd_le(key_in, 0);
    let k1 = rd_le(key_in, 1);
    let k2 = rd_le(key_in, 2);
    let k3 = rd_le(key_in, 3);

    let mut a = k0;
    let mut b = k1;
    let mut c = k2;
    let mut d = k3;

    for i in 0..64usize {
        let j = if i < 16 {
            i
        } else if i < 32 {
            (5 * i + 1) % 16
        } else if i < 48 {
            (3 * i + 5) % 16
        } else {
            (7 * i) % 16
        };

        // Big-endian word read from blockIn (note: NOT the LE block_words view).
        let input = (block_in[4 * j] as u32) << 24
            | (block_in[4 * j + 1] as u32) << 16
            | (block_in[4 * j + 2] as u32) << 8
            | (block_in[4 * j + 3] as u32);

        let mut z = a.wrapping_add(input).wrapping_add(K[i]);
        let mix = if i < 16 {
            f(b, c, d)
        } else if i < 32 {
            g(b, c, d)
        } else if i < 48 {
            h(b, c, d)
        } else {
            i_fn(b, c, d)
        };
        z = rol(z.wrapping_add(mix), SHIFT[i]);
        z = z.wrapping_add(b);

        let tmp = d;
        d = c;
        c = b;
        b = z;
        a = tmp;

        if i == 31 {
            // block_words[] is the little-endian u32 view of block_in.
            let swap = |buf: &mut [u8], x: u32, y: u32| {
                let (x, y) = ((x & 15) as usize, (y & 15) as usize);
                let vx = rd_le(buf, x);
                let vy = rd_le(buf, y);
                wr_le(buf, x, vy);
                wr_le(buf, y, vx);
            };
            swap(&mut block_in, a & 15, b & 15);
            swap(&mut block_in, c & 15, d & 15);
            swap(&mut block_in, (a & (15 << 4)) >> 4, (b & (15 << 4)) >> 4);
            swap(&mut block_in, (a & (15 << 8)) >> 8, (b & (15 << 8)) >> 8);
            swap(
                &mut block_in,
                (a & (15 << 12)) >> 12,
                (b & (15 << 12)) >> 12,
            );
        }
    }

    wr_le(key_out, 0, k0.wrapping_add(a));
    wr_le(key_out, 1, k1.wrapping_add(b));
    wr_le(key_out, 2, k2.wrapping_add(c));
    wr_le(key_out, 3, k3.wrapping_add(d));
}
