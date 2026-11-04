//! Baseline JPEG decoder: 8-bit precision, Huffman coding, one or three
//! components, any sampling factors, restart intervals. Progressive and
//! arithmetic-coded files are rejected.

use std::fmt;

#[derive(Debug)]
pub struct Error(&'static str);

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for Error {}

pub struct Image {
    pub width: usize,
    pub height: usize,
    /// RGBA, row-major, opaque.
    pub pixels: Vec<u8>,
}

const ZIGZAG: [usize; 64] = [
    0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5, 12, 19, 26, 33, 40, 48, 41, 34, 27, 20, 13, 6, 7, 14, 21, 28, 35, 42,
    49, 56, 57, 50, 43, 36, 29, 22, 15, 23, 30, 37, 44, 51, 58, 59, 52, 45, 38, 31, 39, 46, 53, 60, 61, 54, 47, 55, 62, 63,
];

#[derive(Clone, Default)]
struct Huffman {
    /// (code length, code) -> symbol, as canonical tables.
    maxcode: [i32; 18],
    valptr: [i32; 17],
    mincode: [i32; 17],
    values: Vec<u8>,
    /// Fast path: 9-bit prefix -> (length, symbol); length 0 means slow path.
    fast: Vec<(u8, u8)>,
}

const FAST_BITS: u32 = 9;

impl Huffman {
    fn new(counts: &[u8; 16], values: Vec<u8>) -> Huffman {
        let mut h = Huffman { values, fast: vec![(0, 0); 1 << FAST_BITS], ..Default::default() };
        let mut code = 0i32;
        let mut k = 0i32;
        for len in 1..=16 {
            let n = counts[len - 1] as i32;
            h.valptr[len] = k;
            h.mincode[len] = code;
            for i in 0..n {
                if len as u32 <= FAST_BITS {
                    let shift = FAST_BITS - len as u32;
                    let base = ((code + i) << shift) as usize;
                    for j in 0..(1usize << shift) {
                        h.fast[base + j] = (len as u8, h.values[(k + i) as usize]);
                    }
                }
            }
            code += n;
            k += n;
            h.maxcode[len] = if n > 0 { code - 1 } else { -1 };
            code <<= 1;
        }
        h.maxcode[17] = i32::MAX;
        h
    }
}

struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
    acc: u64,
    count: u32,
}

impl Bits<'_> {
    fn fill(&mut self) {
        while self.count <= 56 {
            let mut byte = 0u8;
            if self.pos < self.data.len() {
                byte = self.data[self.pos];
                if byte == 0xff {
                    let next = self.data.get(self.pos + 1).copied().unwrap_or(0);
                    if next == 0 {
                        self.pos += 2;
                    } else {
                        // A marker: feed zeros, do not consume it.
                        byte = 0;
                    }
                } else {
                    self.pos += 1;
                }
            }
            self.acc |= (byte as u64) << (56 - self.count);
            self.count += 8;
        }
    }

    fn peek(&mut self, n: u32) -> u32 {
        if self.count < n {
            self.fill();
        }
        (self.acc >> (64 - n)) as u32
    }

    fn skip(&mut self, n: u32) {
        self.acc <<= n;
        self.count -= n;
    }

    fn get(&mut self, n: u32) -> u32 {
        if n == 0 {
            return 0;
        }
        let v = self.peek(n);
        self.skip(n);
        v
    }

    fn decode(&mut self, h: &Huffman) -> Result<u8, Error> {
        let p = self.peek(FAST_BITS) as usize;
        let (len, sym) = h.fast[p];
        if len > 0 {
            self.skip(len as u32);
            return Ok(sym);
        }
        let mut code = self.get(FAST_BITS) as i32;
        let mut len = FAST_BITS as usize;
        while code > h.maxcode[len] {
            code = (code << 1) | self.get(1) as i32;
            len += 1;
            if len > 16 {
                return Err(Error("bad Huffman code"));
            }
        }
        Ok(h.values[(h.valptr[len] + code - h.mincode[len]) as usize])
    }

    fn receive_extend(&mut self, s: u8) -> i32 {
        if s == 0 {
            return 0;
        }
        let v = self.get(s as u32) as i32;
        if v < (1 << (s - 1)) {
            v - (1 << s) + 1
        } else {
            v
        }
    }

    /// Skips to the byte after the next RSTn marker.
    fn restart(&mut self) {
        self.acc = 0;
        self.count = 0;
        while self.pos + 1 < self.data.len() {
            if self.data[self.pos] == 0xff && (0xd0..=0xd7).contains(&self.data[self.pos + 1]) {
                self.pos += 2;
                return;
            }
            self.pos += 1;
        }
    }
}

struct Component {
    id: u8,
    h: usize,
    v: usize,
    tq: usize,
    td: usize,
    ta: usize,
    /// Decoded samples, `bw * 8` wide.
    plane: Vec<u8>,
    bw: usize,
    pred: i32,
}

fn idct_table() -> [[f32; 8]; 8] {
    let mut t = [[0f32; 8]; 8];
    for (x, row) in t.iter_mut().enumerate() {
        for (u, v) in row.iter_mut().enumerate() {
            let c = if u == 0 { std::f32::consts::FRAC_1_SQRT_2 } else { 1.0 };
            *v = c * (((2 * x + 1) as f32 * u as f32 * std::f32::consts::PI) / 16.0).cos() / 2.0;
        }
    }
    t
}

fn idct(coef: &[i32; 64], q: &[u16; 64], t: &[[f32; 8]; 8], out: &mut [u8], stride: usize) {
    let mut tmp = [0f32; 64];
    let mut deq = [0f32; 64];
    for i in 0..64 {
        deq[i] = (coef[i] * q[i] as i32) as f32;
    }
    for y in 0..8 {
        for x in 0..8 {
            let mut s = 0.0;
            for u in 0..8 {
                s += t[x][u] * deq[y * 8 + u];
            }
            tmp[y * 8 + x] = s;
        }
    }
    for x in 0..8 {
        for y in 0..8 {
            let mut s = 0.0;
            for v in 0..8 {
                s += t[y][v] * tmp[v * 8 + x];
            }
            out[y * stride + x] = (s + 128.5).clamp(0.0, 255.0) as u8;
        }
    }
}

fn be16(d: &[u8], i: usize) -> Result<usize, Error> {
    Ok(u16::from_be_bytes([*d.get(i).ok_or(Error("truncated"))?, *d.get(i + 1).ok_or(Error("truncated"))?]) as usize)
}

pub fn decode(data: &[u8]) -> Result<Image, Error> {
    if data.get(..2) != Some(&[0xff, 0xd8]) {
        return Err(Error("not a JPEG file"));
    }
    let mut qt = [[0u16; 64]; 4];
    let mut dc: [Huffman; 4] = Default::default();
    let mut ac: [Huffman; 4] = Default::default();
    let mut comps: Vec<Component> = Vec::new();
    let (mut width, mut height) = (0, 0);
    let mut restart_interval = 0;
    let mut i = 2;
    loop {
        while data.get(i) == Some(&0xff) && data.get(i + 1) == Some(&0xff) {
            i += 1;
        }
        if data.get(i) != Some(&0xff) {
            return Err(Error("marker expected"));
        }
        let marker = *data.get(i + 1).ok_or(Error("truncated"))?;
        let len = be16(data, i + 2)?;
        let seg = data.get(i + 4..i + 2 + len).ok_or(Error("truncated segment"))?;
        match marker {
            0xdb => {
                let mut p = 0;
                while p < seg.len() {
                    let (precision, id) = (seg[p] >> 4, (seg[p] & 3) as usize);
                    p += 1;
                    for k in 0..64 {
                        let v = if precision == 0 {
                            p += 1;
                            seg[p - 1] as u16
                        } else {
                            p += 2;
                            u16::from_be_bytes([seg[p - 2], seg[p - 1]])
                        };
                        qt[id][ZIGZAG[k]] = v;
                    }
                }
            }
            0xc4 => {
                let mut p = 0;
                while p < seg.len() {
                    let (class, id) = (seg[p] >> 4, (seg[p] & 3) as usize);
                    let counts: [u8; 16] = seg.get(p + 1..p + 17).ok_or(Error("bad DHT"))?.try_into().unwrap();
                    let n: usize = counts.iter().map(|c| *c as usize).sum();
                    let values = seg.get(p + 17..p + 17 + n).ok_or(Error("bad DHT"))?.to_vec();
                    p += 17 + n;
                    let h = Huffman::new(&counts, values);
                    if class == 0 {
                        dc[id] = h;
                    } else {
                        ac[id] = h;
                    }
                }
            }
            0xc0 | 0xc1 => {
                if seg[0] != 8 {
                    return Err(Error("only 8-bit precision is supported"));
                }
                height = be16(seg, 1)?;
                width = be16(seg, 3)?;
                let n = seg[5] as usize;
                if n != 1 && n != 3 {
                    return Err(Error("only grey or YCbCr images are supported"));
                }
                for k in 0..n {
                    let c = &seg[6 + 3 * k..9 + 3 * k];
                    comps.push(Component {
                        id: c[0],
                        h: (c[1] >> 4) as usize,
                        v: (c[1] & 15) as usize,
                        tq: (c[2] & 3) as usize,
                        td: 0,
                        ta: 0,
                        plane: Vec::new(),
                        bw: 0,
                        pred: 0,
                    });
                }
            }
            0xc2 | 0xc3 | 0xc5..=0xc7 | 0xc9..=0xcb | 0xcd..=0xcf => return Err(Error("only baseline JPEG is supported")),
            0xdd => restart_interval = be16(seg, 0)?,
            0xda => {
                let n = seg[0] as usize;
                for k in 0..n {
                    let (id, tables) = (seg[1 + 2 * k], seg[2 + 2 * k]);
                    let c = comps.iter_mut().find(|c| c.id == id).ok_or(Error("unknown component in scan"))?;
                    c.td = (tables >> 4) as usize & 3;
                    c.ta = (tables & 15) as usize & 3;
                }
                if n != comps.len() {
                    return Err(Error("non-interleaved scans are not supported"));
                }
                let scan = &data[i + 2 + len..];
                return decode_scan(scan, width, height, &mut comps, &qt, &dc, &ac, restart_interval);
            }
            0xd9 => return Err(Error("no image data")),
            _ => {}
        }
        i += 2 + len;
    }
}

#[allow(clippy::too_many_arguments)]
fn decode_scan(
    scan: &[u8],
    width: usize,
    height: usize,
    comps: &mut [Component],
    qt: &[[u16; 64]; 4],
    dc: &[Huffman; 4],
    ac: &[Huffman; 4],
    restart_interval: usize,
) -> Result<Image, Error> {
    if width == 0 || height == 0 {
        return Err(Error("empty image"));
    }
    let hmax = comps.iter().map(|c| c.h).max().unwrap();
    let vmax = comps.iter().map(|c| c.v).max().unwrap();
    let mcux = width.div_ceil(8 * hmax);
    let mcuy = height.div_ceil(8 * vmax);
    for c in comps.iter_mut() {
        c.bw = mcux * c.h;
        c.plane = vec![0; c.bw * 8 * mcuy * c.v * 8];
    }
    let t = idct_table();
    let mut bits = Bits { data: scan, pos: 0, acc: 0, count: 0 };
    let mut coef = [0i32; 64];
    let mut mcus = 0;
    for my in 0..mcuy {
        for mx in 0..mcux {
            if restart_interval > 0 && mcus > 0 && mcus % restart_interval == 0 {
                bits.restart();
                for c in comps.iter_mut() {
                    c.pred = 0;
                }
            }
            mcus += 1;
            for c in comps.iter_mut() {
                for by in 0..c.v {
                    for bx in 0..c.h {
                        coef.fill(0);
                        let s = bits.decode(&dc[c.td])?;
                        c.pred += bits.receive_extend(s);
                        coef[0] = c.pred;
                        let mut k = 1;
                        while k < 64 {
                            let rs = bits.decode(&ac[c.ta])?;
                            let (r, s) = ((rs >> 4) as usize, rs & 15);
                            if s == 0 {
                                if r != 15 {
                                    break;
                                }
                                k += 16;
                                continue;
                            }
                            k += r;
                            if k > 63 {
                                return Err(Error("coefficient out of range"));
                            }
                            coef[ZIGZAG[k]] = bits.receive_extend(s);
                            k += 1;
                        }
                        let stride = c.bw * 8;
                        let x0 = (mx * c.h + bx) * 8;
                        let y0 = (my * c.v + by) * 8;
                        idct(&coef, &qt[c.tq], &t, &mut c.plane[y0 * stride + x0..], stride);
                    }
                }
            }
        }
    }
    let mut pixels = vec![255u8; width * height * 4];
    let sample = |c: &Component, x: usize, y: usize| -> f32 {
        let sx = x * c.h / hmax;
        let sy = y * c.v / vmax;
        c.plane[sy * c.bw * 8 + sx] as f32
    };
    for y in 0..height {
        for x in 0..width {
            let p = &mut pixels[(y * width + x) * 4..][..3];
            if comps.len() == 1 {
                let v = sample(&comps[0], x, y) as u8;
                p.copy_from_slice(&[v, v, v]);
            } else {
                let (yy, cb, cr) = (sample(&comps[0], x, y), sample(&comps[1], x, y) - 128.0, sample(&comps[2], x, y) - 128.0);
                p[0] = (yy + 1.402 * cr).round().clamp(0.0, 255.0) as u8;
                p[1] = (yy - 0.344136 * cb - 0.714136 * cr).round().clamp(0.0, 255.0) as u8;
                p[2] = (yy + 1.772 * cb).round().clamp(0.0, 255.0) as u8;
            }
        }
    }
    Ok(Image { width, height, pixels })
}

/// Halves the image with a 2x2 box filter.
pub fn halve(img: &Image) -> Image {
    let (w, h) = (img.width / 2, img.height / 2);
    let mut pixels = vec![255u8; w * h * 4];
    for y in 0..h {
        for x in 0..w {
            for ch in 0..3 {
                let at = |dx: usize, dy: usize| img.pixels[((2 * y + dy) * img.width + 2 * x + dx) * 4 + ch] as u32;
                pixels[(y * w + x) * 4 + ch] = ((at(0, 0) + at(1, 0) + at(0, 1) + at(1, 1) + 2) / 4) as u8;
            }
        }
    }
    Image { width: w, height: h, pixels }
}
