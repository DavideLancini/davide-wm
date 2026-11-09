//! PNG decoder (every color type and bit depth, Adam7) and a minimal RGBA
//! encoder with tEXt chunks.

use crate::inflate::{self, DIST_BASE, DIST_EXTRA, LEN_BASE, LEN_EXTRA};

pub struct Image {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

const SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";
const MAX_SIDE: u32 = 16384;
const MAX_BYTES: u64 = 256 << 20;

/// The chunks after the signature as (type, data), stopping at IEND or
/// truncation.
fn chunks(data: &[u8]) -> impl Iterator<Item = (&[u8], &[u8])> {
    let mut rest = data.strip_prefix(SIGNATURE).unwrap_or(&[]);
    std::iter::from_fn(move || {
        let len = u32::from_be_bytes(rest.get(..4)?.try_into().ok()?) as usize;
        let kind = rest.get(4..8)?;
        let body = rest.get(8..8 + len)?;
        rest = rest.get(12 + len..).unwrap_or(&[]);
        (kind != b"IEND").then_some((kind, body))
    })
}

/// The value of the tEXt chunk with keyword `key`.
pub fn read_text(data: &[u8], key: &str) -> Option<String> {
    chunks(data).filter(|(k, _)| *k == b"tEXt").find_map(|(_, body)| {
        let nul = body.iter().position(|b| *b == 0)?;
        (&body[..nul] == key.as_bytes()).then(|| body[nul + 1..].iter().map(|&b| b as char).collect())
    })
}

/// (x0, y0, dx, dy) of the Adam7 passes.
const ADAM7: [(usize, usize, usize, usize); 7] = [(0, 0, 8, 8), (4, 0, 8, 8), (0, 4, 4, 8), (2, 0, 4, 4), (0, 2, 2, 4), (1, 0, 2, 2), (0, 1, 1, 2)];

fn paeth(a: u8, b: u8, c: u8) -> u8 {
    let p = a as i16 + b as i16 - c as i16;
    let (pa, pb, pc) = ((p - a as i16).abs(), (p - b as i16).abs(), (p - c as i16).abs());
    if pa <= pb && pa <= pc {
        a
    } else if pb <= pc {
        b
    } else {
        c
    }
}

fn unfilter(kind: u8, row: &mut [u8], prev: &[u8], bpp: usize) -> Result<(), String> {
    match kind {
        0 => {}
        1 => (bpp..row.len()).for_each(|i| row[i] = row[i].wrapping_add(row[i - bpp])),
        2 => row.iter_mut().zip(prev).for_each(|(x, p)| *x = x.wrapping_add(*p)),
        3 => (0..row.len()).for_each(|i| {
            let left = if i >= bpp { row[i - bpp] as u16 } else { 0 };
            row[i] = row[i].wrapping_add(((left + prev[i] as u16) / 2) as u8);
        }),
        4 => (0..row.len()).for_each(|i| {
            let (a, c) = if i >= bpp { (row[i - bpp], prev[i - bpp]) } else { (0, 0) };
            row[i] = row[i].wrapping_add(paeth(a, prev[i], c));
        }),
        _ => return Err("filtro non valido".into()),
    }
    Ok(())
}

fn sample(row: &[u8], i: usize, depth: u8) -> u16 {
    match depth {
        8 => row[i] as u16,
        16 => u16::from_be_bytes([row[2 * i], row[2 * i + 1]]),
        d => {
            let bit = i * d as usize;
            ((row[bit / 8] >> (8 - d as usize - bit % 8)) & ((1 << d) - 1)) as u16
        }
    }
}

struct Format {
    color: u8,
    depth: u8,
    channels: usize,
    palette: Vec<[u8; 4]>,
    /// Transparent gray or RGB sample at native depth.
    key: Option<[u16; 3]>,
}

impl Format {
    fn row_bytes(&self, width: usize) -> usize {
        (width * self.channels * self.depth as usize).div_ceil(8)
    }

    fn pixel(&self, row: &[u8], x: usize) -> [u8; 4] {
        let s = |k: usize| sample(row, x * self.channels + k, self.depth);
        let to8 = |v: u16| match self.depth {
            16 => (v >> 8) as u8,
            8 => v as u8,
            d => (v as u32 * 255 / ((1 << d) - 1)) as u8,
        };
        match self.color {
            0 => {
                let v = s(0);
                let a = if self.key.is_some_and(|k| k[0] == v) { 0 } else { 255 };
                let g = to8(v);
                [g, g, g, a]
            }
            2 => {
                let v = [s(0), s(1), s(2)];
                [to8(v[0]), to8(v[1]), to8(v[2]), if self.key == Some(v) { 0 } else { 255 }]
            }
            3 => self.palette.get(s(0) as usize).copied().unwrap_or([0, 0, 0, 255]),
            4 => {
                let g = to8(s(0));
                [g, g, g, to8(s(1))]
            }
            _ => [to8(s(0)), to8(s(1)), to8(s(2)), to8(s(3))],
        }
    }
}

pub fn decode(data: &[u8]) -> Result<Image, String> {
    if !data.starts_with(SIGNATURE) {
        return Err("non è un PNG".into());
    }
    let mut ihdr = None;
    let mut palette = Vec::new();
    let mut trns: &[u8] = &[];
    let mut idat = Vec::new();
    for (kind, body) in chunks(data) {
        match kind {
            b"IHDR" if body.len() == 13 => ihdr = Some(body),
            b"PLTE" => palette = body.as_chunks::<3>().0.iter().map(|c| [c[0], c[1], c[2], 255]).collect(),
            b"tRNS" => trns = body,
            b"IDAT" => idat.extend_from_slice(body),
            _ => {}
        }
    }
    let h = ihdr.ok_or("manca IHDR")?;
    let (width, height) = (u32::from_be_bytes([h[0], h[1], h[2], h[3]]), u32::from_be_bytes([h[4], h[5], h[6], h[7]]));
    let (depth, color, interlaced) = (h[8], h[9], h[12] == 1);
    let channels = match (color, depth) {
        (0, 1 | 2 | 4 | 8 | 16) => 1,
        (3, 1 | 2 | 4 | 8) => 1,
        (2, 8 | 16) => 3,
        (4, 8 | 16) => 2,
        (6, 8 | 16) => 4,
        _ => return Err("formato PNG non supportato".into()),
    };
    if width == 0 || height == 0 || width > MAX_SIDE || height > MAX_SIDE || width as u64 * height as u64 * 4 > MAX_BYTES || h[10] != 0 || h[11] != 0 || h[12] > 1 {
        return Err("PNG troppo grande o non valido".into());
    }
    if color == 3 {
        if palette.is_empty() {
            return Err("manca la palette".into());
        }
        for (p, a) in palette.iter_mut().zip(trns) {
            p[3] = *a;
        }
    }
    let word = |i: usize| trns.get(2 * i..2 * i + 2).map(|w| u16::from_be_bytes([w[0], w[1]]));
    let key = match color {
        0 => word(0).map(|g| [g, 0, 0]),
        2 => (|| Some([word(0)?, word(1)?, word(2)?]))(),
        _ => None,
    };
    let fmt = Format { color, depth, channels, palette, key };
    let (w, ht) = (width as usize, height as usize);
    let passes: Vec<_> = if interlaced {
        ADAM7.iter().map(|&(x0, y0, dx, dy)| (x0, y0, dx, dy, (w + dx - 1 - x0) / dx, (ht + dy - 1 - y0) / dy)).collect()
    } else {
        vec![(0, 0, 1, 1, w, ht)]
    };
    let raw_len: usize = passes.iter().filter(|p| p.4 > 0).map(|p| p.5 * (1 + fmt.row_bytes(p.4))).sum();
    let raw = inflate::zlib(&idat, raw_len)?;
    if raw.len() < raw_len {
        return Err("dati dell'immagine troncati".into());
    }
    let bpp = (channels * depth as usize).div_ceil(8);
    let mut rgba = vec![0u8; w * ht * 4];
    let mut pos = 0;
    for &(x0, y0, dx, dy, pw, ph) in &passes {
        if pw == 0 {
            continue;
        }
        let stride = fmt.row_bytes(pw);
        let mut prev = vec![0u8; stride];
        let mut row = vec![0u8; stride];
        for py in 0..ph {
            let kind = raw[pos];
            row.copy_from_slice(&raw[pos + 1..pos + 1 + stride]);
            pos += 1 + stride;
            unfilter(kind, &mut row, &prev, bpp)?;
            let y = y0 + py * dy;
            for px in 0..pw {
                let at = (y * w + x0 + px * dx) * 4;
                rgba[at..at + 4].copy_from_slice(&fmt.pixel(&row, px));
            }
            std::mem::swap(&mut row, &mut prev);
        }
    }
    Ok(Image { width, height, rgba })
}

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

fn matched(b: &mut Bits, len: usize, dist: usize) {
    let l = LEN_BASE.iter().rposition(|base| *base as usize <= len).unwrap_or(0);
    literal(b, 257 + l as u32);
    b.put((len - LEN_BASE[l] as usize) as u32, LEN_EXTRA[l] as u32);
    let d = DIST_BASE.iter().rposition(|base| *base as usize <= dist).unwrap_or(0);
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

/// Encodes `rgba` (rows top to bottom) as an 8-bit RGBA PNG with the
/// (Latin-1) `text` key/value pairs as tEXt chunks.
pub fn encode(width: u32, height: u32, rgba: &[u8], text: &[(&str, &str)]) -> Vec<u8> {
    let mut raw = Vec::with_capacity((width as usize * 4 + 1) * height as usize);
    for row in rgba.chunks_exact((width as usize * 4).max(1)).take(height as usize) {
        raw.push(0);
        raw.extend_from_slice(row);
    }
    let mut z = vec![0x78, 0x01];
    z.extend(deflate(&raw));
    z.extend_from_slice(&adler32(&raw).to_be_bytes());
    let mut out = SIGNATURE.to_vec();
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
    chunk(&mut out, b"IHDR", &ihdr);
    for (k, v) in text {
        let mut body: Vec<u8> = k.bytes().collect();
        body.push(0);
        body.extend(v.chars().map(|c| if (c as u32) < 256 { c as u8 } else { b'?' }));
        chunk(&mut out, b"tEXt", &body);
    }
    chunk(&mut out, b"IDAT", &z);
    chunk(&mut out, b"IEND", &[]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checksums() {
        assert_eq!(crc32(b"IEND"), 0xae42_6082);
        assert_eq!(adler32(b"Wikipedia"), 0x11e6_0398);
    }

    #[test]
    fn round_trip() {
        let (w, h) = (37u32, 23u32);
        let rgba: Vec<u8> = (0..w * h * 4).map(|i| if i % 97 < 50 { 7 } else { (i * 31 % 251) as u8 }).collect();
        let png = encode(w, h, &rgba, &[("Thumb::MTime", "123"), ("Software", "davide-wm")]);
        let img = decode(&png).unwrap();
        assert_eq!((img.width, img.height), (w, h));
        assert_eq!(img.rgba, rgba);
        assert_eq!(read_text(&png, "Thumb::MTime").as_deref(), Some("123"));
        assert_eq!(read_text(&png, "Software").as_deref(), Some("davide-wm"));
        assert_eq!(read_text(&png, "Missing"), None);
        for cut in [0, 8, 20, 40, png.len() / 2, png.len() - 20] {
            assert!(decode(&png[..cut]).is_err());
        }
        let mut bad = png.clone();
        bad[16..20].copy_from_slice(&100_000u32.to_be_bytes());
        assert!(decode(&bad).is_err());
    }

    /// Stored-deflate zlib stream, enough to build test images.
    fn zlib_stored(raw: &[u8]) -> Vec<u8> {
        let mut z = vec![0x78, 0x01];
        let blocks: Vec<&[u8]> = raw.chunks(65535).collect();
        for (i, b) in blocks.iter().enumerate() {
            z.push((i + 1 == blocks.len()) as u8);
            z.extend_from_slice(&(b.len() as u16).to_le_bytes());
            z.extend_from_slice(&(!(b.len() as u16)).to_le_bytes());
            z.extend_from_slice(b);
        }
        z.extend_from_slice(&adler32(raw).to_be_bytes());
        z
    }

    /// Builds a PNG from packed rows, applying filter `y % 5` to row y.
    fn build(w: u32, h: u32, color: u8, depth: u8, interlace: bool, packed: impl Fn(usize, usize) -> Vec<u8>, extra: &[(&[u8; 4], Vec<u8>)]) -> Vec<u8> {
        let mut raw = Vec::new();
        let passes: Vec<_> = if interlace { ADAM7.to_vec() } else { vec![(0, 0, 1, 1)] };
        let channels = [1, 0, 3, 1, 2, 0, 4][color as usize];
        let bpp = (channels * depth as usize).div_ceil(8);
        for (x0, y0, dx, dy) in passes {
            let (pw, ph) = ((w as usize + dx - 1 - x0) / dx, (h as usize + dy - 1 - y0) / dy);
            if pw == 0 {
                continue;
            }
            let rows: Vec<Vec<u8>> = (0..ph).map(|py| packed(y0 + py * dy, x0 * 1000 + dx)).collect();
            let mut prev = vec![0u8; rows[0].len()];
            for (y, row) in rows.iter().enumerate() {
                let kind = (y % 5) as u8;
                raw.push(kind);
                for i in 0..row.len() {
                    let a = if i >= bpp { row[i - bpp] } else { 0 };
                    let c = if i >= bpp { prev[i - bpp] } else { 0 };
                    let pred = match kind {
                        0 => 0,
                        1 => a,
                        2 => prev[i],
                        3 => ((a as u16 + prev[i] as u16) / 2) as u8,
                        _ => paeth(a, prev[i], c),
                    };
                    raw.push(row[i].wrapping_sub(pred));
                }
                prev = row.clone();
            }
        }
        let mut out = SIGNATURE.to_vec();
        let mut ihdr = w.to_be_bytes().to_vec();
        ihdr.extend_from_slice(&h.to_be_bytes());
        ihdr.extend_from_slice(&[depth, color, 0, 0, interlace as u8]);
        chunk(&mut out, b"IHDR", &ihdr);
        for (k, v) in extra {
            chunk(&mut out, k, v);
        }
        chunk(&mut out, b"IDAT", &zlib_stored(&raw));
        chunk(&mut out, b"IEND", &[]);
        out
    }

    #[test]
    fn color_types_filters_interlace() {
        let (w, h) = (13u32, 11u32);
        let v = |x: usize, y: usize| (x * 7 + y * 13) as u32;
        // Rows are given as (y, x0 * 1000 + dx) so pixels can be recomputed per pass.
        let xs = move |key: usize| {
            let (x0, dx) = (key / 1000, key % 1000);
            (x0..w as usize).step_by(dx).collect::<Vec<_>>()
        };
        for interlace in [false, true] {
            let png = build(w, h, 0, 2, interlace, |y, k| {
                let mut row = vec![0u8; xs(k).len().div_ceil(4)];
                for (i, x) in xs(k).into_iter().enumerate() {
                    row[i / 4] |= ((v(x, y) % 4) as u8) << (6 - 2 * (i % 4));
                }
                row
            }, &[]);
            let img = decode(&png).unwrap();
            for y in 0..h as usize {
                for x in 0..w as usize {
                    let g = (v(x, y) % 4 * 85) as u8;
                    assert_eq!(img.rgba[(y * w as usize + x) * 4..][..4], [g, g, g, 255], "gray2 {x},{y} {interlace}");
                }
            }
            let png = build(w, h, 6, 16, interlace, |y, k| xs(k).into_iter().flat_map(|x| (0..4).flat_map(move |c| ((v(x, y) * 300 + c * 999) as u16).to_be_bytes())).collect(), &[]);
            let img = decode(&png).unwrap();
            for y in 0..h as usize {
                for x in 0..w as usize {
                    let want: Vec<u8> = (0..4).map(|c| ((v(x, y) * 300 + c * 999) as u16 >> 8) as u8).collect();
                    assert_eq!(img.rgba[(y * w as usize + x) * 4..][..4], want[..], "rgba16 {x},{y} {interlace}");
                }
            }
        }
        let pal = (b"PLTE", vec![10, 20, 30, 40, 50, 60]);
        let trns = (b"tRNS", vec![128]);
        let png = build(3, 1, 3, 1, false, |_, _| vec![0b0100_0000], &[pal, trns]);
        assert_eq!(decode(&png).unwrap().rgba, [10, 20, 30, 128, 40, 50, 60, 255, 10, 20, 30, 128]);
        let png = build(2, 1, 2, 8, false, |_, _| vec![1, 2, 3, 4, 5, 6], &[(b"tRNS", vec![0, 4, 0, 5, 0, 6])]);
        assert_eq!(decode(&png).unwrap().rgba, [1, 2, 3, 255, 4, 5, 6, 0]);
        let png = build(2, 1, 4, 8, false, |_, _| vec![9, 100, 200, 50], &[]);
        assert_eq!(decode(&png).unwrap().rgba, [9, 9, 9, 100, 200, 200, 200, 50]);
    }

    #[test]
    fn system_files() {
        let mut checked = 0;
        for dir in ["/usr/lib/go/src/image/png/testdata/pngsuite", "/usr/share/pixmaps", "/usr/share/icons/hicolor/48x48/apps"] {
            let Ok(rd) = std::fs::read_dir(dir) else { continue };
            for e in rd.flatten().take(300) {
                let p = e.path();
                if p.extension().is_none_or(|x| x != "png") {
                    continue;
                }
                let Ok(data) = std::fs::read(&p) else { continue };
                let corrupt = p.file_name().is_some_and(|n| n.as_encoded_bytes().starts_with(b"x"));
                match decode(&data) {
                    Ok(img) => {
                        assert_eq!(img.width, u32::from_be_bytes(data[16..20].try_into().unwrap()), "{p:?}");
                        assert_eq!(img.height, u32::from_be_bytes(data[20..24].try_into().unwrap()), "{p:?}");
                        assert_eq!(img.rgba.len(), (img.width * img.height * 4) as usize);
                        checked += 1;
                    }
                    Err(e) => assert!(corrupt, "{p:?}: {e}"),
                }
            }
        }
        eprintln!("decoded {checked} system PNGs");
    }
}
