//! JPEG decoding: baseline and progressive Huffman-coded JPEG (JFIF/EXIF),
//! 1, 3 or 4 components, any chroma subsampling, restart intervals.
//!
//! All scans are decoded into per-component coefficient buffers first, so
//! baseline and progressive files share the same reconstruction path:
//! dequantise, inverse DCT (integer, after stb_image), upsample, convert
//! colour.

use alloc::vec;
use alloc::vec::Vec;

use crate::{Image, ImageError};

const ZIGZAG: [usize; 64] = [
    0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5, 12, 19, 26, 33, 40, 48, 41, 34, 27, 20, 13, 6, 7, 14, 21, 28, 35, 42,
    49, 56, 57, 50, 43, 36, 29, 22, 15, 23, 30, 37, 44, 51, 58, 59, 52, 45, 38, 31, 39, 46, 53, 60, 61, 54, 47, 55, 62, 63,
];

/// The standard tables from Annex K of the JPEG specification. Motion-
/// JPEG streams often omit their DHT segments and rely on these.
const STD_DC_COUNTS: [[u8; 16]; 2] = [
    [0, 1, 5, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0],
    [0, 3, 1, 1, 1, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0],
];
const STD_DC_VALUES: [u8; 12] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11];
const STD_AC_COUNTS: [[u8; 16]; 2] = [
    [0, 2, 1, 3, 3, 2, 4, 3, 5, 5, 4, 4, 0, 0, 1, 0x7d],
    [0, 2, 1, 2, 4, 4, 3, 4, 7, 5, 4, 4, 0, 1, 2, 0x77],
];
const STD_AC_LUMA: [u8; 162] = [
    0x01, 0x02, 0x03, 0x00, 0x04, 0x11, 0x05, 0x12, 0x21, 0x31, 0x41, 0x06, 0x13, 0x51, 0x61, 0x07, 0x22, 0x71, 0x14, 0x32, 0x81,
    0x91, 0xa1, 0x08, 0x23, 0x42, 0xb1, 0xc1, 0x15, 0x52, 0xd1, 0xf0, 0x24, 0x33, 0x62, 0x72, 0x82, 0x09, 0x0a, 0x16, 0x17, 0x18,
    0x19, 0x1a, 0x25, 0x26, 0x27, 0x28, 0x29, 0x2a, 0x34, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3a, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48,
    0x49, 0x4a, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5a, 0x63, 0x64, 0x65, 0x66, 0x67, 0x68, 0x69, 0x6a, 0x73, 0x74, 0x75,
    0x76, 0x77, 0x78, 0x79, 0x7a, 0x83, 0x84, 0x85, 0x86, 0x87, 0x88, 0x89, 0x8a, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97, 0x98, 0x99,
    0x9a, 0xa2, 0xa3, 0xa4, 0xa5, 0xa6, 0xa7, 0xa8, 0xa9, 0xaa, 0xb2, 0xb3, 0xb4, 0xb5, 0xb6, 0xb7, 0xb8, 0xb9, 0xba, 0xc2, 0xc3,
    0xc4, 0xc5, 0xc6, 0xc7, 0xc8, 0xc9, 0xca, 0xd2, 0xd3, 0xd4, 0xd5, 0xd6, 0xd7, 0xd8, 0xd9, 0xda, 0xe1, 0xe2, 0xe3, 0xe4, 0xe5,
    0xe6, 0xe7, 0xe8, 0xe9, 0xea, 0xf1, 0xf2, 0xf3, 0xf4, 0xf5, 0xf6, 0xf7, 0xf8, 0xf9, 0xfa,
];
const STD_AC_CHROMA: [u8; 162] = [
    0x00, 0x01, 0x02, 0x03, 0x11, 0x04, 0x05, 0x21, 0x31, 0x06, 0x12, 0x41, 0x51, 0x07, 0x61, 0x71, 0x13, 0x22, 0x32, 0x81, 0x08,
    0x14, 0x42, 0x91, 0xa1, 0xb1, 0xc1, 0x09, 0x23, 0x33, 0x52, 0xf0, 0x15, 0x62, 0x72, 0xd1, 0x0a, 0x16, 0x24, 0x34, 0xe1, 0x25,
    0xf1, 0x17, 0x18, 0x19, 0x1a, 0x26, 0x27, 0x28, 0x29, 0x2a, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3a, 0x43, 0x44, 0x45, 0x46, 0x47,
    0x48, 0x49, 0x4a, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5a, 0x63, 0x64, 0x65, 0x66, 0x67, 0x68, 0x69, 0x6a, 0x73, 0x74,
    0x75, 0x76, 0x77, 0x78, 0x79, 0x7a, 0x82, 0x83, 0x84, 0x85, 0x86, 0x87, 0x88, 0x89, 0x8a, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97,
    0x98, 0x99, 0x9a, 0xa2, 0xa3, 0xa4, 0xa5, 0xa6, 0xa7, 0xa8, 0xa9, 0xaa, 0xb2, 0xb3, 0xb4, 0xb5, 0xb6, 0xb7, 0xb8, 0xb9, 0xba,
    0xc2, 0xc3, 0xc4, 0xc5, 0xc6, 0xc7, 0xc8, 0xc9, 0xca, 0xd2, 0xd3, 0xd4, 0xd5, 0xd6, 0xd7, 0xd8, 0xd9, 0xda, 0xe2, 0xe3, 0xe4,
    0xe5, 0xe6, 0xe7, 0xe8, 0xe9, 0xea, 0xf2, 0xf3, 0xf4, 0xf5, 0xf6, 0xf7, 0xf8, 0xf9, 0xfa,
];

pub fn is_jpeg(data: &[u8]) -> bool {
    data.len() > 3 && data[0] == 0xff && data[1] == 0xd8 && data[2] == 0xff
}

#[derive(Clone)]
struct Huffman {
    /// Fast lookup on the next 9 bits: (length << 8) | value, 0 = miss.
    fast: [u16; 512],
    maxcode: [i32; 18],
    valptr: [i32; 17],
    mincode: [i32; 17],
    values: Vec<u8>,
}

impl Huffman {
    fn new(counts: &[u8; 16], values: &[u8]) -> Result<Huffman, ImageError> {
        let mut h = Huffman { fast: [0; 512], maxcode: [-1; 18], valptr: [0; 17], mincode: [0; 17], values: values.to_vec() };
        let mut code: i32 = 0;
        let mut k: i32 = 0;
        for len in 1..=16usize {
            let n = counts[len - 1] as i32;
            h.valptr[len] = k;
            h.mincode[len] = code;
            if n > 0 {
                for _ in 0..n {
                    if len <= 9 {
                        let shift = 9 - len;
                        let base = (code as usize) << shift;
                        if base + (1 << shift) > 512 || k as usize >= values.len() {
                            return Err(ImageError::Corrupt);
                        }
                        for f in 0..(1 << shift) {
                            h.fast[base + f] = ((len as u16) << 8) | values[k as usize] as u16;
                        }
                    }
                    code += 1;
                    k += 1;
                }
                h.maxcode[len] = code - 1;
            }
            code <<= 1;
        }
        h.maxcode[17] = i32::MAX;
        if k as usize > values.len() {
            return Err(ImageError::Corrupt);
        }
        Ok(h)
    }
}

struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
    buf: u32,
    count: u32,
    /// A marker was reached; further reads return zero bits.
    marker: bool,
}

impl<'a> Bits<'a> {
    fn fill(&mut self) {
        while self.count <= 24 {
            let mut b = 0u32;
            if !self.marker && self.pos < self.data.len() {
                let v = self.data[self.pos];
                if v == 0xff {
                    let next = self.data.get(self.pos + 1).copied().unwrap_or(0);
                    if next == 0x00 {
                        self.pos += 2;
                        b = 0xff;
                    } else {
                        self.marker = true;
                    }
                } else {
                    self.pos += 1;
                    b = v as u32;
                }
            }
            self.buf |= b << (24 - self.count);
            self.count += 8;
        }
    }

    fn bits(&mut self, n: u32) -> u32 {
        if n == 0 {
            return 0;
        }
        self.fill();
        let v = self.buf >> (32 - n);
        self.buf <<= n;
        self.count -= n;
        v
    }

    fn bit(&mut self) -> bool {
        self.bits(1) != 0
    }

    fn decode(&mut self, h: &Huffman) -> Result<u8, ImageError> {
        self.fill();
        let e = h.fast[(self.buf >> 23) as usize];
        if e != 0 {
            let len = (e >> 8) as u32;
            self.buf <<= len;
            self.count -= len;
            return Ok(e as u8);
        }
        let mut code: i32 = 0;
        for len in 1..=16usize {
            code = (code << 1) | self.bits(1) as i32;
            if h.maxcode[len] >= code && code >= h.mincode[len] && h.maxcode[len] >= 0 {
                let idx = h.valptr[len] + code - h.mincode[len];
                return h.values.get(idx as usize).copied().ok_or(ImageError::Corrupt);
            }
        }
        Err(ImageError::Corrupt)
    }

    /// Skip to just after the next RSTn marker.
    fn restart(&mut self) {
        self.buf = 0;
        self.count = 0;
        self.marker = false;
        while self.pos + 1 < self.data.len() {
            if self.data[self.pos] == 0xff && (0xd0..=0xd7).contains(&self.data[self.pos + 1]) {
                self.pos += 2;
                return;
            }
            self.pos += 1;
        }
    }
}

fn extend(v: u32, s: u32) -> i32 {
    if s == 0 {
        0
    } else if v < (1 << (s - 1)) {
        v as i32 - (1 << s) + 1
    } else {
        v as i32
    }
}

struct Component {
    id: u8,
    h: usize,
    v: usize,
    tq: usize,
    /// Blocks per line / column, padded to whole MCUs.
    bw: usize,
    bh: usize,
    coeffs: Vec<i16>,
    dc_pred: i32,
    dc_table: usize,
    ac_table: usize,
}

struct Decoder<'a> {
    data: &'a [u8],
    qt: [[u16; 64]; 4],
    dc: [Option<Huffman>; 4],
    ac: [Option<Huffman>; 4],
    comps: Vec<Component>,
    width: usize,
    height: usize,
    hmax: usize,
    vmax: usize,
    mcux: usize,
    mcuy: usize,
    progressive: bool,
    restart_interval: usize,
    eobrun: u32,
    adobe_transform: Option<u8>,
}

fn be16(b: &[u8], o: usize) -> usize {
    ((b[o] as usize) << 8) | b[o + 1] as usize
}

impl<'a> Decoder<'a> {
    fn decode_block_baseline(&mut self, bits: &mut Bits, ci: usize, bx: usize, by: usize) -> Result<(), ImageError> {
        let c = &mut self.comps[ci];
        let dc = self.dc[c.dc_table].as_ref().ok_or(ImageError::Corrupt)?;
        let ac = self.ac[c.ac_table].as_ref().ok_or(ImageError::Corrupt)?;
        let base = (by * c.bw + bx) * 64;
        let blk = &mut c.coeffs[base..base + 64];
        let t = bits.decode(dc)? as u32;
        let diff = extend(bits.bits(t), t);
        c.dc_pred += diff;
        blk[0] = c.dc_pred as i16;
        let mut k = 1;
        while k < 64 {
            let rs = bits.decode(ac)?;
            let (r, s) = ((rs >> 4) as usize, (rs & 15) as u32);
            if s == 0 {
                if rs != 0xf0 {
                    break;
                }
                k += 16;
            } else {
                k += r;
                if k > 63 {
                    return Err(ImageError::Corrupt);
                }
                blk[ZIGZAG[k]] = extend(bits.bits(s), s) as i16;
                k += 1;
            }
        }
        Ok(())
    }

    fn decode_block_progressive(
        &mut self,
        bits: &mut Bits,
        ci: usize,
        bx: usize,
        by: usize,
        ss: usize,
        se: usize,
        ah: u32,
        al: u32,
    ) -> Result<(), ImageError> {
        let c = &mut self.comps[ci];
        let base = (by * c.bw + bx) * 64;
        if ss == 0 {
            // DC scan.
            let blk = &mut c.coeffs[base..base + 64];
            if ah == 0 {
                let dc = self.dc[c.dc_table].as_ref().ok_or(ImageError::Corrupt)?;
                let t = bits.decode(dc)? as u32;
                c.dc_pred += extend(bits.bits(t), t);
                blk[0] = (c.dc_pred * (1 << al)) as i16;
            } else if bits.bit() {
                blk[0] |= 1 << al;
            }
            return Ok(());
        }
        let ac = self.ac[c.ac_table].as_ref().ok_or(ImageError::Corrupt)?;
        let blk = &mut c.coeffs[base..base + 64];
        if ah == 0 {
            // First AC scan for this band.
            if self.eobrun > 0 {
                self.eobrun -= 1;
                return Ok(());
            }
            let mut k = ss;
            while k <= se {
                let rs = bits.decode(ac)?;
                let (r, s) = ((rs >> 4) as u32, (rs & 15) as u32);
                if s == 0 {
                    if r < 15 {
                        self.eobrun = (1 << r) - 1;
                        if r > 0 {
                            self.eobrun += bits.bits(r);
                        }
                        break;
                    }
                    k += 16;
                } else {
                    k += r as usize;
                    if k > 63 {
                        return Err(ImageError::Corrupt);
                    }
                    blk[ZIGZAG[k]] = (extend(bits.bits(s), s) * (1 << al)) as i16;
                    k += 1;
                }
            }
            return Ok(());
        }
        // Refinement scan (after stb_image).
        let bit = 1i16 << al;
        if self.eobrun > 0 {
            self.eobrun -= 1;
            for k in ss..=se {
                let p = &mut blk[ZIGZAG[k]];
                if *p != 0 && bits.bit() && *p & bit == 0 {
                    *p += if *p > 0 { bit } else { -bit };
                }
            }
            return Ok(());
        }
        let mut k = ss;
        while k <= se {
            let rs = bits.decode(ac)?;
            let mut r = (rs >> 4) as i32;
            let s = (rs & 15) as u32;
            let mut val: i16 = 0;
            if s == 0 {
                if r < 15 {
                    self.eobrun = (1 << r) - 1;
                    if r > 0 {
                        self.eobrun += bits.bits(r as u32);
                    }
                    r = 64;
                }
            } else {
                val = if bits.bit() { bit } else { -bit };
            }
            while k <= se {
                let p = &mut blk[ZIGZAG[k]];
                k += 1;
                if *p != 0 {
                    if bits.bit() && *p & bit == 0 {
                        *p += if *p > 0 { bit } else { -bit };
                    }
                } else {
                    if r == 0 {
                        *p = val;
                        break;
                    }
                    r -= 1;
                }
            }
        }
        Ok(())
    }

    fn scan(&mut self, header: &[u8], start: usize) -> Result<usize, ImageError> {
        let n = header[0] as usize;
        if n == 0 || n > 4 || header.len() < 1 + n * 2 + 3 {
            return Err(ImageError::Corrupt);
        }
        let mut sel = Vec::new();
        for i in 0..n {
            let id = header[1 + i * 2];
            let tables = header[2 + i * 2];
            let ci = self.comps.iter().position(|c| c.id == id).ok_or(ImageError::Corrupt)?;
            self.comps[ci].dc_table = (tables >> 4) as usize & 3;
            self.comps[ci].ac_table = (tables & 15) as usize & 3;
            sel.push(ci);
        }
        let ss = header[1 + n * 2] as usize;
        let se = (header[2 + n * 2] as usize).min(63);
        let ah = (header[3 + n * 2] >> 4) as u32;
        let al = (header[3 + n * 2] & 15) as u32;
        for &ci in &sel {
            self.comps[ci].dc_pred = 0;
        }
        self.eobrun = 0;
        let mut bits = Bits { data: self.data, pos: start, buf: 0, count: 0, marker: false };
        let mut todo = if self.restart_interval > 0 { self.restart_interval } else { usize::MAX };

        let after_unit = |d: &mut Decoder, bits: &mut Bits, todo: &mut usize| {
            *todo -= 1;
            if *todo == 0 && d.restart_interval > 0 {
                bits.restart();
                *todo = d.restart_interval;
                for c in d.comps.iter_mut() {
                    c.dc_pred = 0;
                }
                d.eobrun = 0;
            }
        };

        if sel.len() == 1 {
            // Non-interleaved: the component's own blocks, not MCU padded.
            let ci = sel[0];
            let (ch, cv) = (self.comps[ci].h, self.comps[ci].v);
            let cw = (self.width * ch).div_ceil(self.hmax);
            let chh = (self.height * cv).div_ceil(self.vmax);
            let (bw, bh) = (cw.div_ceil(8), chh.div_ceil(8));
            for by in 0..bh {
                for bx in 0..bw {
                    if self.progressive {
                        self.decode_block_progressive(&mut bits, ci, bx, by, ss, se, ah, al)?;
                    } else {
                        self.decode_block_baseline(&mut bits, ci, bx, by)?;
                    }
                    after_unit(self, &mut bits, &mut todo);
                }
            }
        } else {
            for my in 0..self.mcuy {
                for mx in 0..self.mcux {
                    for &ci in &sel {
                        let (ch, cv) = (self.comps[ci].h, self.comps[ci].v);
                        for v in 0..cv {
                            for h in 0..ch {
                                let (bx, by) = (mx * ch + h, my * cv + v);
                                if self.progressive {
                                    self.decode_block_progressive(&mut bits, ci, bx, by, ss, se, ah, al)?;
                                } else {
                                    self.decode_block_baseline(&mut bits, ci, bx, by)?;
                                }
                            }
                        }
                    }
                    after_unit(self, &mut bits, &mut todo);
                }
            }
        }
        // Find the next marker after the entropy-coded data.
        let mut p = bits.pos;
        while p + 1 < self.data.len() {
            if self.data[p] == 0xff && self.data[p + 1] != 0 && !(0xd0..=0xd7).contains(&self.data[p + 1]) {
                return Ok(p);
            }
            p += 1;
        }
        Ok(self.data.len())
    }
}

const C0_541: i32 = 2217;
const C1_847: i32 = -7567;
const C0_765: i32 = 3135;
const C1_175: i32 = 4816;
const C0_298: i32 = 1223;
const C2_053: i32 = 8410;
const C3_072: i32 = 12586;
const C1_501: i32 = 6149;
const C0_899: i32 = -3685;
const C2_562: i32 = -10497;
const C1_961: i32 = -8034;
const C0_390: i32 = -1597;

#[inline]
fn idct_1d(s: [i32; 8]) -> ([i32; 4], [i32; 4]) {
    let (mut p2, mut p3) = (s[2], s[6]);
    let mut p1 = (p2 + p3) * C0_541;
    let t2 = p1 + p3 * C1_847;
    let t3 = p1 + p2 * C0_765;
    p2 = s[0];
    p3 = s[4];
    let t0 = (p2 + p3) << 12;
    let t1 = (p2 - p3) << 12;
    let x = [t0 + t3, t1 + t2, t1 - t2, t0 - t3];
    let (mut t0, mut t1, mut t2, mut t3) = (s[7], s[5], s[3], s[1]);
    p3 = t0 + t2;
    let mut p4 = t1 + t3;
    p1 = t0 + t3;
    p2 = t1 + t2;
    let p5 = (p3 + p4) * C1_175;
    t0 *= C0_298;
    t1 *= C2_053;
    t2 *= C3_072;
    t3 *= C1_501;
    p1 = p5 + p1 * C0_899;
    p2 = p5 + p2 * C2_562;
    p3 *= C1_961;
    p4 *= C0_390;
    t3 += p1 + p4;
    t2 += p2 + p3;
    t1 += p2 + p4;
    t0 += p1 + p3;
    (x, [t0, t1, t2, t3])
}

/// Inverse DCT of dequantised coefficients (natural order) into 8x8
/// samples at `out` with `stride`.
fn idct_block(d: &[i32; 64], out: &mut [u8], stride: usize) {
    let mut v = [0i32; 64];
    for i in 0..8 {
        if (1..8).all(|r| d[r * 8 + i] == 0) {
            let dc = d[i] * 4;
            for r in 0..8 {
                v[r * 8 + i] = dc;
            }
            continue;
        }
        let s = [d[i], d[8 + i], d[16 + i], d[24 + i], d[32 + i], d[40 + i], d[48 + i], d[56 + i]];
        let (mut x, t) = idct_1d(s);
        for xi in x.iter_mut() {
            *xi += 512;
        }
        v[i] = (x[0] + t[3]) >> 10;
        v[56 + i] = (x[0] - t[3]) >> 10;
        v[8 + i] = (x[1] + t[2]) >> 10;
        v[48 + i] = (x[1] - t[2]) >> 10;
        v[16 + i] = (x[2] + t[1]) >> 10;
        v[40 + i] = (x[2] - t[1]) >> 10;
        v[24 + i] = (x[3] + t[0]) >> 10;
        v[32 + i] = (x[3] - t[0]) >> 10;
    }
    let clamp = |x: i32| x.clamp(0, 255) as u8;
    for r in 0..8 {
        let row = &v[r * 8..r * 8 + 8];
        let (mut x, t) = idct_1d([row[0], row[1], row[2], row[3], row[4], row[5], row[6], row[7]]);
        for xi in x.iter_mut() {
            *xi += 65536 + (128 << 17);
        }
        let o = &mut out[r * stride..r * stride + 8];
        o[0] = clamp((x[0] + t[3]) >> 17);
        o[7] = clamp((x[0] - t[3]) >> 17);
        o[1] = clamp((x[1] + t[2]) >> 17);
        o[6] = clamp((x[1] - t[2]) >> 17);
        o[2] = clamp((x[2] + t[1]) >> 17);
        o[5] = clamp((x[2] - t[1]) >> 17);
        o[3] = clamp((x[3] + t[0]) >> 17);
        o[4] = clamp((x[3] - t[0]) >> 17);
    }
}

pub fn decode(data: &[u8]) -> Result<Image, ImageError> {
    if !is_jpeg(data) {
        return Err(ImageError::Unsupported);
    }
    let mut d = Decoder {
        data,
        qt: [[1; 64]; 4],
        dc: [None, None, None, None],
        ac: [None, None, None, None],
        comps: Vec::new(),
        width: 0,
        height: 0,
        hmax: 1,
        vmax: 1,
        mcux: 0,
        mcuy: 0,
        progressive: false,
        restart_interval: 0,
        eobrun: 0,
        adobe_transform: None,
    };
    let mut pos = 2;
    let mut seen_frame = false;
    loop {
        // Find the next marker.
        while pos < data.len() && data[pos] != 0xff {
            pos += 1;
        }
        while pos < data.len() && data[pos] == 0xff {
            pos += 1;
        }
        if pos >= data.len() {
            break;
        }
        let marker = data[pos];
        pos += 1;
        match marker {
            0xd8 | 0x01 | 0xd0..=0xd7 => continue,
            0xd9 => break,
            _ => {}
        }
        if pos + 2 > data.len() {
            return Err(ImageError::Truncated);
        }
        let len = be16(data, pos);
        if len < 2 || pos + len > data.len() {
            return Err(ImageError::Truncated);
        }
        let seg = &data[pos + 2..pos + len];
        match marker {
            0xdb => {
                let mut i = 0;
                while i < seg.len() {
                    let pq = seg[i] >> 4;
                    let tq = (seg[i] & 3) as usize;
                    i += 1;
                    for k in 0..64 {
                        let v = if pq == 0 {
                            *seg.get(i + k).ok_or(ImageError::Truncated)? as u16
                        } else {
                            u16::from_be_bytes([*seg.get(i + k * 2).ok_or(ImageError::Truncated)?, seg[i + k * 2 + 1]])
                        };
                        d.qt[tq][ZIGZAG[k]] = v;
                    }
                    i += if pq == 0 { 64 } else { 128 };
                }
            }
            0xc4 => {
                let mut i = 0;
                while i + 17 <= seg.len() {
                    let class = seg[i] >> 4;
                    let th = (seg[i] & 3) as usize;
                    let mut counts = [0u8; 16];
                    counts.copy_from_slice(&seg[i + 1..i + 17]);
                    let total: usize = counts.iter().map(|&c| c as usize).sum();
                    let vals = seg.get(i + 17..i + 17 + total).ok_or(ImageError::Truncated)?;
                    let h = Huffman::new(&counts, vals)?;
                    if class == 0 {
                        d.dc[th] = Some(h);
                    } else {
                        d.ac[th] = Some(h);
                    }
                    i += 17 + total;
                }
            }
            0xc0 | 0xc1 | 0xc2 => {
                if seg.len() < 6 || seg[0] != 8 {
                    return Err(ImageError::Unsupported); // 12-bit precision
                }
                d.progressive = marker == 0xc2;
                d.height = be16(seg, 1);
                d.width = be16(seg, 3);
                let n = seg[5] as usize;
                if d.width == 0 || d.height == 0 || !(n == 1 || n == 3 || n == 4) || seg.len() < 6 + n * 3 {
                    return Err(ImageError::Unsupported);
                }
                if d.width as u64 * d.height as u64 > 50_000_000 {
                    return Err(ImageError::TooLarge);
                }
                for i in 0..n {
                    let c = &seg[6 + i * 3..9 + i * 3];
                    let (h, v) = ((c[1] >> 4) as usize, (c[1] & 15) as usize);
                    if h == 0 || v == 0 || h > 4 || v > 4 {
                        return Err(ImageError::Corrupt);
                    }
                    d.comps.push(Component {
                        id: c[0],
                        h,
                        v,
                        tq: (c[2] & 3) as usize,
                        bw: 0,
                        bh: 0,
                        coeffs: Vec::new(),
                        dc_pred: 0,
                        dc_table: 0,
                        ac_table: 0,
                    });
                }
                d.hmax = d.comps.iter().map(|c| c.h).max().unwrap();
                d.vmax = d.comps.iter().map(|c| c.v).max().unwrap();
                d.mcux = d.width.div_ceil(8 * d.hmax);
                d.mcuy = d.height.div_ceil(8 * d.vmax);
                for c in d.comps.iter_mut() {
                    c.bw = d.mcux * c.h;
                    c.bh = d.mcuy * c.v;
                    c.coeffs = vec![0; c.bw * c.bh * 64];
                }
                seen_frame = true;
            }
            0xc3 | 0xc5..=0xc7 | 0xc9..=0xcb | 0xcd..=0xcf => return Err(ImageError::Unsupported),
            0xdd => d.restart_interval = be16(seg, 0),
            0xee => {
                if seg.len() >= 12 && &seg[..5] == b"Adobe" {
                    d.adobe_transform = Some(seg[11]);
                }
            }
            0xda => {
                if !seen_frame {
                    return Err(ImageError::Corrupt);
                }
                for t in 0..2 {
                    if d.dc[t].is_none() {
                        d.dc[t] = Some(Huffman::new(&STD_DC_COUNTS[t], &STD_DC_VALUES)?);
                    }
                    if d.ac[t].is_none() {
                        let vals: &[u8] = if t == 0 { &STD_AC_LUMA } else { &STD_AC_CHROMA };
                        d.ac[t] = Some(Huffman::new(&STD_AC_COUNTS[t], vals)?);
                    }
                }
                pos = d.scan(seg, pos + len)?;
                continue;
            }
            _ => {}
        }
        pos += len;
    }
    if !seen_frame {
        return Err(ImageError::Corrupt);
    }
    finish(&d)
}

fn finish(d: &Decoder) -> Result<Image, ImageError> {
    // Reconstruct each component plane.
    let mut planes: Vec<(Vec<u8>, usize)> = Vec::new();
    for c in &d.comps {
        let stride = c.bw * 8;
        let mut plane = vec![0u8; stride * c.bh * 8];
        let q = &d.qt[c.tq];
        let mut deq = [0i32; 64];
        for by in 0..c.bh {
            for bx in 0..c.bw {
                let base = (by * c.bw + bx) * 64;
                for k in 0..64 {
                    deq[k] = c.coeffs[base + k] as i32 * q[k] as i32;
                }
                let off = by * 8 * stride + bx * 8;
                idct_block(&deq, &mut plane[off..], stride);
            }
        }
        planes.push((plane, stride));
    }
    let mut img = Image::new(d.width as u32, d.height as u32);
    let n = d.comps.len();
    // Bring every component to full resolution. Subsampled chroma is
    // interpolated linearly between sample centres (libjpeg's "fancy"
    // triangle upsampling for 2x).
    let (w, h) = (d.width, d.height);
    let full: Vec<Vec<u8>> = d
        .comps
        .iter()
        .zip(planes.iter())
        .map(|(c, (p, stride))| {
            if c.h == d.hmax && c.v == d.vmax {
                let mut out = vec![0u8; w * h];
                for y in 0..h {
                    out[y * w..(y + 1) * w].copy_from_slice(&p[y * stride..y * stride + w]);
                }
                return out;
            }
            let cw = (w * c.h).div_ceil(d.hmax).max(1);
            let chh = (h * c.v).div_ceil(d.vmax).max(1);
            // Source coordinate (8.8 fixed point) for each output column/row.
            let map = |o: usize, num: usize, den: usize, max: usize| -> (usize, usize, u32) {
                let s = ((2 * o + 1) * num * 256 / (2 * den)) as i64 - 128;
                let s = s.clamp(0, ((max - 1) * 256) as i64) as usize;
                let i0 = s >> 8;
                (i0, (i0 + 1).min(max - 1), (s & 255) as u32)
            };
            let xs: Vec<(usize, usize, u32)> = (0..w).map(|x| map(x, c.h, d.hmax, cw)).collect();
            let mut out = vec![0u8; w * h];
            for y in 0..h {
                let (y0, y1, fy) = map(y, c.v, d.vmax, chh);
                let r0 = &p[y0 * stride..];
                let r1 = &p[y1 * stride..];
                for (x, &(x0, x1, fx)) in xs.iter().enumerate() {
                    let top = r0[x0] as u32 * (256 - fx) + r0[x1] as u32 * fx;
                    let bot = r1[x0] as u32 * (256 - fx) + r1[x1] as u32 * fx;
                    out[y * w + x] = ((top * (256 - fy) + bot * fy + 32768) >> 16) as u8;
                }
            }
            out
        })
        .collect();
    let sample = |ci: usize, x: usize, y: usize| -> i32 { full[ci][y * w + x] as i32 };
    // Colour transform: YCbCr for 3 components unless an Adobe marker says
    // RGB; 4 components are CMYK or (Adobe transform 2) YCCK.
    let ycc = match n {
        3 => d.adobe_transform != Some(0),
        4 => d.adobe_transform == Some(2),
        _ => false,
    };
    for y in 0..d.height {
        for x in 0..d.width {
            let rgba = match n {
                1 => {
                    let g = sample(0, x, y) as u8;
                    [g, g, g, 255]
                }
                _ => {
                    let (c0, c1, c2) = (sample(0, x, y), sample(1, x, y), sample(2, x, y));
                    let (r, g, b) = if ycc {
                        let (cb, cr) = (c1 - 128, c2 - 128);
                        (
                            c0 + ((91881 * cr) >> 16),
                            c0 - ((22554 * cb + 46802 * cr) >> 16),
                            c0 + ((116130 * cb) >> 16),
                        )
                    } else {
                        (c0, c1, c2)
                    };
                    let (r, g, b) = (r.clamp(0, 255), g.clamp(0, 255), b.clamp(0, 255));
                    if n == 4 {
                        // Adobe CMYK is stored inverted.
                        let k = sample(3, x, y);
                        [(r * k / 255) as u8, (g * k / 255) as u8, (b * k / 255) as u8, 255]
                    } else {
                        [r as u8, g as u8, b as u8, 255]
                    }
                }
            };
            img.set(x as u32, y as u32, rgba);
        }
    }
    Ok(img)
}
