//! zlib/deflate decompression (RFC 1950/1951): stored, fixed and dynamic
//! Huffman blocks.

const FAST: u32 = 10;

struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
    buf: u64,
    n: u32,
}

impl Bits<'_> {
    fn refill(&mut self) {
        while self.n <= 56 && self.pos < self.data.len() {
            self.buf |= (self.data[self.pos] as u64) << self.n;
            self.pos += 1;
            self.n += 8;
        }
    }

    fn get(&mut self, count: u32) -> Result<u32, String> {
        if self.n < count {
            self.refill();
            if self.n < count {
                return Err("dati compressi troncati".into());
            }
        }
        let v = (self.buf & ((1u64 << count) - 1)) as u32;
        self.buf >>= count;
        self.n -= count;
        Ok(v)
    }

    /// Drops the partial byte and hands back whole buffered bytes.
    fn align(&mut self) {
        let whole = self.n / 8;
        self.pos -= whole as usize;
        self.buf = 0;
        self.n = 0;
    }
}

struct Huffman {
    counts: [u16; 16],
    symbols: Vec<u16>,
    /// Indexed by the next FAST bits: symbol | length << 9, 0 if longer.
    fast: Vec<u16>,
}

impl Huffman {
    fn new(lengths: &[u8]) -> Result<Huffman, String> {
        let mut counts = [0u16; 16];
        for &l in lengths {
            counts[l as usize] += 1;
        }
        counts[0] = 0;
        let mut left = 1i32;
        for &c in &counts[1..] {
            left = (left << 1) - c as i32;
            if left < 0 {
                return Err("codici di Huffman non validi".into());
            }
        }
        let mut offs = [0u16; 16];
        for l in 1..15 {
            offs[l + 1] = offs[l] + counts[l];
        }
        let mut symbols = vec![0u16; lengths.len()];
        let mut next = [0u32; 16];
        let mut code = 0u32;
        for l in 1..16 {
            code = (code + counts[l - 1] as u32) << 1;
            next[l] = code;
        }
        let mut fast = vec![0u16; 1 << FAST];
        for (sym, &l) in lengths.iter().enumerate() {
            if l == 0 {
                continue;
            }
            let l = l as usize;
            symbols[offs[l] as usize] = sym as u16;
            offs[l] += 1;
            let c = next[l];
            next[l] += 1;
            if l as u32 <= FAST {
                let rev = c.reverse_bits() >> (32 - l);
                for j in (rev as usize..1 << FAST).step_by(1 << l) {
                    fast[j] = sym as u16 | (l as u16) << 9;
                }
            }
        }
        Ok(Huffman { counts, symbols, fast })
    }

    fn decode(&self, b: &mut Bits) -> Result<u16, String> {
        if b.n < 15 {
            b.refill();
        }
        let e = self.fast[(b.buf & ((1 << FAST) - 1)) as usize];
        let len = (e >> 9) as u32;
        if e != 0 && len <= b.n {
            b.buf >>= len;
            b.n -= len;
            return Ok(e & 0x1ff);
        }
        let (mut code, mut first, mut index) = (0i32, 0i32, 0i32);
        for l in 1..16 {
            code |= b.get(1)? as i32;
            let count = self.counts[l] as i32;
            if code - first < count {
                return Ok(self.symbols[(index + code - first) as usize]);
            }
            index += count;
            first = (first + count) << 1;
            code <<= 1;
        }
        Err("codice di Huffman non valido".into())
    }
}

pub(crate) const LEN_BASE: [u16; 29] = [3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131, 163, 195, 227, 258];
pub(crate) const LEN_EXTRA: [u8; 29] = [0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0];
pub(crate) const DIST_BASE: [u16; 30] =
    [1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537, 2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577];
pub(crate) const DIST_EXTRA: [u8; 30] = [0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13, 13];
const ORDER: [usize; 19] = [16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15];

fn dynamic(b: &mut Bits) -> Result<(Huffman, Huffman), String> {
    let nlit = b.get(5)? as usize + 257;
    let ndist = b.get(5)? as usize + 1;
    let ncode = b.get(4)? as usize + 4;
    let mut cl = [0u8; 19];
    for &i in &ORDER[..ncode] {
        cl[i] = b.get(3)? as u8;
    }
    let clh = Huffman::new(&cl)?;
    let mut lengths = Vec::with_capacity(nlit + ndist);
    while lengths.len() < nlit + ndist {
        let (val, rep) = match clh.decode(b)? {
            s @ 0..=15 => (s as u8, 1),
            16 => (*lengths.last().ok_or("ripetizione senza codice")?, 3 + b.get(2)?),
            17 => (0, 3 + b.get(3)?),
            18 => (0, 11 + b.get(7)?),
            _ => return Err("lunghezza di codice non valida".into()),
        };
        if lengths.len() + rep as usize > nlit + ndist {
            return Err("troppe lunghezze di codice".into());
        }
        lengths.extend(std::iter::repeat_n(val, rep as usize));
    }
    if lengths[256] == 0 {
        return Err("manca il codice di fine blocco".into());
    }
    Ok((Huffman::new(&lengths[..nlit])?, Huffman::new(&lengths[nlit..])?))
}

fn fixed() -> (Huffman, Huffman) {
    let mut l = [8u8; 288];
    l[144..256].fill(9);
    l[256..280].fill(7);
    (Huffman::new(&l).unwrap(), Huffman::new(&[5; 30]).unwrap())
}

/// Inflates a raw deflate stream; fails once the output would exceed `limit`.
pub fn inflate(data: &[u8], limit: usize) -> Result<Vec<u8>, String> {
    let mut b = Bits { data, pos: 0, buf: 0, n: 0 };
    let mut out: Vec<u8> = Vec::new();
    loop {
        let last = b.get(1)? == 1;
        match b.get(2)? {
            0 => {
                b.align();
                let hdr = data.get(b.pos..b.pos + 4).ok_or("blocco troncato")?;
                let len = u16::from_le_bytes([hdr[0], hdr[1]]) as usize;
                if len != !u16::from_le_bytes([hdr[2], hdr[3]]) as usize {
                    return Err("blocco non compresso corrotto".into());
                }
                b.pos += 4;
                let chunk = data.get(b.pos..b.pos + len).ok_or("blocco troncato")?;
                if out.len() + len > limit {
                    return Err("dati decompressi troppo grandi".into());
                }
                out.extend_from_slice(chunk);
                b.pos += len;
            }
            t @ (1 | 2) => {
                let (lit, dist) = if t == 1 { fixed() } else { dynamic(&mut b)? };
                loop {
                    let sym = lit.decode(&mut b)? as usize;
                    if sym < 256 {
                        if out.len() >= limit {
                            return Err("dati decompressi troppo grandi".into());
                        }
                        out.push(sym as u8);
                        continue;
                    }
                    if sym == 256 {
                        break;
                    }
                    let l = sym - 257;
                    if l >= 29 {
                        return Err("lunghezza non valida".into());
                    }
                    let len = LEN_BASE[l] as usize + b.get(LEN_EXTRA[l] as u32)? as usize;
                    let d = dist.decode(&mut b)? as usize;
                    if d >= 30 {
                        return Err("distanza non valida".into());
                    }
                    let dist = DIST_BASE[d] as usize + b.get(DIST_EXTRA[d] as u32)? as usize;
                    if dist > out.len() {
                        return Err("distanza oltre l'inizio".into());
                    }
                    if out.len() + len > limit {
                        return Err("dati decompressi troppo grandi".into());
                    }
                    let start = out.len() - dist;
                    if dist >= len {
                        out.extend_from_within(start..start + len);
                    } else {
                        for i in 0..len {
                            out.push(out[start + i]);
                        }
                    }
                }
            }
            _ => return Err("tipo di blocco non valido".into()),
        }
        if last {
            return Ok(out);
        }
    }
}

/// Inflates a zlib stream (the checksum is not verified).
pub fn zlib(data: &[u8], limit: usize) -> Result<Vec<u8>, String> {
    match data {
        [cmf, flg, rest @ ..] if cmf & 0x0f == 8 && (*cmf as u16 * 256 + *flg as u16).is_multiple_of(31) && flg & 0x20 == 0 => inflate(rest, limit),
        _ => Err("intestazione zlib non valida".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stored_and_fixed() {
        // zlib.compress(b"hello hello hello", level=0) and level=9.
        let stored = [0x78, 0x01, 0x01, 0x05, 0x00, 0xfa, 0xff, b'h', b'e', b'l', b'l', b'o'];
        assert_eq!(zlib(&stored, 100).unwrap(), b"hello");
        let fixed = [0xcb, 0x48, 0xcd, 0xc9, 0xc9, 0x57, 0xc8, 0x40, 0x90, 0x00];
        assert_eq!(inflate(&fixed, 100).unwrap(), b"hello hello hello");
        assert!(inflate(&fixed, 10).is_err());
        assert!(inflate(&fixed[..5], 100).is_err());
        assert!(zlib(&[0x78], 100).is_err());
    }

    #[test]
    fn garbage_does_not_panic() {
        let mut x = 12345u32;
        for len in 0..400 {
            let data: Vec<u8> = (0..len)
                .map(|_| {
                    x = x.wrapping_mul(1_103_515_245).wrapping_add(12345);
                    (x >> 16) as u8
                })
                .collect();
            let _ = inflate(&data, 1 << 16);
        }
    }
}
