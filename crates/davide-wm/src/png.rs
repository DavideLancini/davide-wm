//! Minimal PNG encoder: RGBA, no filtering, deflate with LZ77 and the fixed
//! Huffman codes (screenshots are mostly flat areas, which this catches).

fn crc32(data: &[u8]) -> u32 {
    let mut table = [0u32; 256];
    for (i, t) in table.iter_mut().enumerate() {
        let mut c = i as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 { 0xedb8_8320 ^ (c >> 1) } else { c >> 1 };
        }
        *t = c;
    }
    !data.iter().fold(!0u32, |c, b| table[((c ^ *b as u32) & 0xff) as usize] ^ (c >> 8))
}

fn adler32(data: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for chunk in data.chunks(5552) {
        for x in chunk {
            a += *x as u32;
            b += a;
        }
        a %= 65521;
        b %= 65521;
    }
    b << 16 | a
}

/// Writes bits least significant first, as deflate wants.
struct Bits {
    out: Vec<u8>,
    acc: u64,
    n: u32,
}

impl Bits {
    fn put(&mut self, value: u32, count: u32) {
        self.acc |= (value as u64) << self.n;
        self.n += count;
        while self.n >= 8 {
            self.out.push(self.acc as u8);
            self.acc >>= 8;
            self.n -= 8;
        }
    }

    /// Huffman codes go most significant bit first.
    fn code(&mut self, code: u32, len: u32) {
        self.put(code.reverse_bits() >> (32 - len), len);
    }

    fn finish(mut self) -> Vec<u8> {
        if self.n > 0 {
            self.out.push(self.acc as u8);
        }
        self.out
    }
}

fn literal(b: &mut Bits, sym: u32) {
    match sym {
        0..=143 => b.code(0x30 + sym, 8),
        144..=255 => b.code(0x190 + sym - 144, 9),
        256..=279 => b.code(sym - 256, 7),
        _ => b.code(0xc0 + sym - 280, 8),
    }
}

const LEN_BASE: [u16; 29] = [3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131, 163, 195, 227, 258];
const LEN_EXTRA: [u8; 29] = [0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0];
const DIST_BASE: [u16; 30] =
    [1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537, 2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577];
const DIST_EXTRA: [u8; 30] = [0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13, 13];

fn matched(b: &mut Bits, len: usize, dist: usize) {
    let l = LEN_BASE.iter().rposition(|base| *base as usize <= len).unwrap();
    literal(b, 257 + l as u32);
    b.put((len - LEN_BASE[l] as usize) as u32, LEN_EXTRA[l] as u32);
    let d = DIST_BASE.iter().rposition(|base| *base as usize <= dist).unwrap();
    b.code(d as u32, 5);
    b.put((dist - DIST_BASE[d] as usize) as u32, DIST_EXTRA[d] as u32);
}

const WINDOW: usize = 32768;
const HASH_BITS: u32 = 15;
const MAX_CHAIN: usize = 24;

/// One fixed-Huffman deflate block.
fn deflate(data: &[u8]) -> Vec<u8> {
    let mut b = Bits { out: Vec::with_capacity(data.len() / 4), acc: 0, n: 0 };
    b.put(1, 1);
    b.put(1, 2);
    let mut head = vec![usize::MAX; 1 << HASH_BITS];
    let mut prev = vec![usize::MAX; WINDOW];
    let hash = |i: usize| (u32::from_le_bytes([data[i], data[i + 1], data[i + 2], 0]).wrapping_mul(2_654_435_761) >> (32 - HASH_BITS)) as usize;
    let insert = |i: usize, head: &mut [usize], prev: &mut [usize]| {
        if i + 3 <= data.len() {
            let h = hash(i);
            prev[i % WINDOW] = head[h];
            head[h] = i;
        }
    };
    let mut i = 0;
    while i < data.len() {
        let (mut best_len, mut best_dist) = (0, 0);
        if i + 3 <= data.len() {
            let mut cand = head[hash(i)];
            let max = (data.len() - i).min(258);
            for _ in 0..MAX_CHAIN {
                if cand == usize::MAX || i - cand > WINDOW - 1 || cand >= i {
                    break;
                }
                let len = data[cand..].iter().zip(&data[i..i + max]).take_while(|(a, b)| a == b).count();
                if len > best_len {
                    (best_len, best_dist) = (len, i - cand);
                    if len == max {
                        break;
                    }
                }
                let p = prev[cand % WINDOW];
                if p == usize::MAX || p >= cand {
                    break;
                }
                cand = p;
            }
        }
        if best_len >= 3 {
            matched(&mut b, best_len, best_dist);
            for j in i..i + best_len {
                insert(j, &mut head, &mut prev);
            }
            i += best_len;
        } else {
            literal(&mut b, data[i] as u32);
            insert(i, &mut head, &mut prev);
            i += 1;
        }
    }
    literal(&mut b, 256);
    b.finish()
}

fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    let start = out.len();
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let crc = crc32(&out[start..]);
    out.extend_from_slice(&crc.to_be_bytes());
}

/// Encodes `rgba` (rows top to bottom) as a PNG file.
pub fn encode(width: u32, height: u32, rgba: &[u8]) -> Vec<u8> {
    let mut raw = Vec::with_capacity((width as usize * 4 + 1) * height as usize);
    for row in rgba.chunks_exact(width as usize * 4) {
        raw.push(0);
        raw.extend_from_slice(row);
    }
    let mut z = vec![0x78, 0x01];
    z.extend(deflate(&raw));
    z.extend_from_slice(&adler32(&raw).to_be_bytes());
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
    chunk(&mut out, b"IHDR", &ihdr);
    chunk(&mut out, b"IDAT", &z);
    chunk(&mut out, b"IEND", &[]);
    out
}

/// A rectangle of an RGBA image of width `w`.
pub fn crop(rgba: &[u8], w: u32, (x, y, cw, ch): (u32, u32, u32, u32)) -> Vec<u8> {
    let mut out = Vec::with_capacity((cw * ch * 4) as usize);
    for row in y..y + ch {
        let start = ((row * w + x) * 4) as usize;
        out.extend_from_slice(&rgba[start..start + (cw * 4) as usize]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Inflates a fixed-Huffman stream: enough to check the encoder.
    fn inflate(z: &[u8]) -> Vec<u8> {
        let mut pos = 0usize;
        let mut bit = |n: u32| -> u32 {
            let mut v = 0;
            for k in 0..n {
                v |= (((z[pos / 8] >> (pos % 8)) & 1) as u32) << k;
                pos += 1;
            }
            v
        };
        assert_eq!(bit(3), 0b011);
        let mut out: Vec<u8> = Vec::new();
        let code = |bit: &mut dyn FnMut(u32) -> u32, len: u32| (0..len).fold(0, |c, _| c << 1 | bit(1));
        loop {
            let mut c = code(&mut bit, 7);
            let sym = if c <= 0x17 {
                c + 256
            } else {
                c = c << 1 | bit(1);
                if (0x30..=0xbf).contains(&c) {
                    c - 0x30
                } else if (0xc0..=0xc7).contains(&c) {
                    c - 0xc0 + 280
                } else {
                    (c << 1 | bit(1)) - 0x190 + 144
                }
            };
            match sym {
                0..=255 => out.push(sym as u8),
                256 => return out,
                s => {
                    let l = (s - 257) as usize;
                    let len = LEN_BASE[l] as usize + bit(LEN_EXTRA[l] as u32) as usize;
                    let d = code(&mut bit, 5) as usize;
                    let dist = DIST_BASE[d] as usize + bit(DIST_EXTRA[d] as u32) as usize;
                    for _ in 0..len {
                        out.push(out[out.len() - dist]);
                    }
                }
            }
        }
    }

    #[test]
    fn checksums() {
        assert_eq!(crc32(b"IEND"), 0xae42_6082);
        assert_eq!(adler32(b"Wikipedia"), 0x11e6_0398);
    }

    #[test]
    fn deflate_round_trip() {
        let mut data: Vec<u8> = (0..5000u32).map(|i| (i * 7 % 251) as u8).collect();
        data.extend(std::iter::repeat_n(9u8, 70_000));
        data.extend_from_slice(b"abcabcabcabcxyz");
        let z = deflate(&data);
        assert!(z.len() < data.len() / 4);
        assert_eq!(inflate(&z), data);
    }
}
