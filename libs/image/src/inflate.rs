//! DEFLATE (RFC 1951) and zlib (RFC 1950) decompression.

use alloc::vec::Vec;

#[derive(Debug, PartialEq, Eq)]
pub enum InflateError {
    Truncated,
    BadBlockType,
    BadStoredLength,
    BadHuffmanTable,
    BadSymbol,
    BadDistance,
    BadZlibHeader,
    TooLarge,
}

struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
    bitbuf: u64,
    bitcount: u32,
}

impl<'a> Bits<'a> {
    fn new(data: &'a [u8]) -> Self {
        Bits { data, pos: 0, bitbuf: 0, bitcount: 0 }
    }

    fn refill(&mut self) {
        while self.bitcount <= 56 {
            let b = if self.pos < self.data.len() { self.data[self.pos] } else { 0 };
            if self.pos <= self.data.len() + 8 {
                self.pos += 1;
            }
            self.bitbuf |= (b as u64) << self.bitcount;
            self.bitcount += 8;
        }
    }

    fn need(&mut self, n: u32) -> Result<(), InflateError> {
        if self.bitcount < n {
            self.refill();
        }
        // Reading far past the end means the stream is truncated.
        if self.pos > self.data.len() + 8 {
            return Err(InflateError::Truncated);
        }
        Ok(())
    }

    fn bits(&mut self, n: u32) -> Result<u32, InflateError> {
        if n == 0 {
            return Ok(0);
        }
        self.need(n)?;
        let v = (self.bitbuf & ((1u64 << n) - 1)) as u32;
        self.bitbuf >>= n;
        self.bitcount -= n;
        Ok(v)
    }

    fn align_byte(&mut self) {
        let drop = self.bitcount % 8;
        self.bitbuf >>= drop;
        self.bitcount -= drop;
    }

    /// Bytes consumed so far (whole bytes not still in the bit buffer).
    fn byte_pos(&self) -> usize {
        self.pos - (self.bitcount / 8) as usize
    }
}

/// Canonical Huffman decoding table with a 9-bit fast lookup.
struct Huffman {
    fast: [u16; 512],
    // For codes longer than 9 bits: per-length first code / symbol index.
    counts: [u16; 16],
    symbols: Vec<u16>,
}

const FAST_BITS: u32 = 9;

impl Huffman {
    fn new(lengths: &[u8]) -> Result<Huffman, InflateError> {
        let mut counts = [0u16; 16];
        for &l in lengths {
            counts[l as usize] += 1;
        }
        counts[0] = 0;
        let mut offs = [0u16; 16];
        for i in 1..16 {
            offs[i] = offs[i - 1] + counts[i - 1];
        }
        let mut symbols = alloc::vec![0u16; lengths.len()];
        for (s, &l) in lengths.iter().enumerate() {
            if l != 0 {
                symbols[offs[l as usize] as usize] = s as u16;
                offs[l as usize] += 1;
            }
        }
        // Check the code isn't over-subscribed.
        let mut left: i32 = 1;
        for &c in counts.iter().skip(1) {
            left <<= 1;
            left -= c as i32;
            if left < 0 {
                return Err(InflateError::BadHuffmanTable);
            }
        }
        let mut h = Huffman { fast: [0xffff; 512], counts, symbols };
        // Fill the fast table: entry = symbol | length << 12.
        let mut code: u32 = 0;
        let mut idx = 0usize;
        for len in 1..=FAST_BITS {
            for _ in 0..h.counts[len as usize] {
                let sym = h.symbols[idx];
                idx += 1;
                // Codes are stored bit-reversed in the stream.
                let rev = reverse(code, len);
                let mut k = rev;
                while k < 512 {
                    h.fast[k as usize] = sym | (len as u16) << 12;
                    k += 1 << len;
                }
                code += 1;
            }
            code <<= 1;
        }
        Ok(h)
    }

    fn decode(&self, b: &mut Bits) -> Result<u16, InflateError> {
        b.need(16)?;
        let e = self.fast[(b.bitbuf & 511) as usize];
        if e != 0xffff {
            let len = (e >> 12) as u32;
            b.bitbuf >>= len;
            b.bitcount -= len;
            return Ok(e & 0xfff);
        }
        // Slow path: canonical decoding bit by bit.
        let mut code: i32 = 0;
        let mut first: i32 = 0;
        let mut index: i32 = 0;
        for len in 1..16 {
            code |= b.bits(1)? as i32;
            let count = self.counts[len] as i32;
            if code - count < first {
                return Ok(self.symbols[(index + (code - first)) as usize]);
            }
            index += count;
            first += count;
            first <<= 1;
            code <<= 1;
        }
        Err(InflateError::BadSymbol)
    }
}

fn reverse(mut code: u32, len: u32) -> u32 {
    let mut r = 0;
    for _ in 0..len {
        r = (r << 1) | (code & 1);
        code >>= 1;
    }
    r
}

const LEN_BASE: [u16; 29] = [3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131, 163, 195, 227, 258];
const LEN_EXTRA: [u8; 29] = [0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0];
const DIST_BASE: [u16; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537, 2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
];
const DIST_EXTRA: [u8; 30] = [0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13, 13];

/// Decompress a raw DEFLATE stream. `limit` caps the output size.
pub fn inflate(data: &[u8], limit: usize) -> Result<Vec<u8>, InflateError> {
    inflate_with_pos(data, limit).map(|(v, _)| v)
}

fn inflate_with_pos(data: &[u8], limit: usize) -> Result<(Vec<u8>, usize), InflateError> {
    let mut out: Vec<u8> = Vec::new();
    let mut b = Bits::new(data);
    loop {
        let last = b.bits(1)?;
        match b.bits(2)? {
            0 => {
                b.align_byte();
                let len = b.bits(16)? as usize;
                let nlen = b.bits(16)? as usize;
                if len != !nlen & 0xffff {
                    return Err(InflateError::BadStoredLength);
                }
                for _ in 0..len {
                    out.push(b.bits(8)? as u8);
                }
            }
            1 => {
                let mut l = [0u8; 288];
                l[..144].fill(8);
                l[144..256].fill(9);
                l[256..280].fill(7);
                l[280..].fill(8);
                let lit = Huffman::new(&l)?;
                let dist = Huffman::new(&[5u8; 30])?;
                block(&mut b, &mut out, &lit, &dist, limit)?;
            }
            2 => {
                let hlit = b.bits(5)? as usize + 257;
                let hdist = b.bits(5)? as usize + 1;
                let hclen = b.bits(4)? as usize + 4;
                const ORDER: [usize; 19] = [16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15];
                let mut cl = [0u8; 19];
                for &o in ORDER.iter().take(hclen) {
                    cl[o] = b.bits(3)? as u8;
                }
                let clh = Huffman::new(&cl)?;
                let mut lengths = alloc::vec![0u8; hlit + hdist];
                let mut i = 0;
                while i < hlit + hdist {
                    let sym = clh.decode(&mut b)?;
                    match sym {
                        0..=15 => {
                            lengths[i] = sym as u8;
                            i += 1;
                        }
                        16 => {
                            if i == 0 {
                                return Err(InflateError::BadHuffmanTable);
                            }
                            let prev = lengths[i - 1];
                            for _ in 0..3 + b.bits(2)? {
                                if i >= lengths.len() {
                                    return Err(InflateError::BadHuffmanTable);
                                }
                                lengths[i] = prev;
                                i += 1;
                            }
                        }
                        17 | 18 => {
                            let n = if sym == 17 { 3 + b.bits(3)? } else { 11 + b.bits(7)? };
                            for _ in 0..n {
                                if i >= lengths.len() {
                                    return Err(InflateError::BadHuffmanTable);
                                }
                                lengths[i] = 0;
                                i += 1;
                            }
                        }
                        _ => return Err(InflateError::BadHuffmanTable),
                    }
                }
                let lit = Huffman::new(&lengths[..hlit])?;
                let dist = Huffman::new(&lengths[hlit..])?;
                block(&mut b, &mut out, &lit, &dist, limit)?;
            }
            _ => return Err(InflateError::BadBlockType),
        }
        if out.len() > limit {
            return Err(InflateError::TooLarge);
        }
        if last == 1 {
            break;
        }
    }
    b.align_byte();
    Ok((out, b.byte_pos()))
}

fn block(b: &mut Bits, out: &mut Vec<u8>, lit: &Huffman, dist: &Huffman, limit: usize) -> Result<(), InflateError> {
    loop {
        let sym = lit.decode(b)?;
        if sym < 256 {
            out.push(sym as u8);
        } else if sym == 256 {
            return Ok(());
        } else {
            let s = (sym - 257) as usize;
            if s >= 29 {
                return Err(InflateError::BadSymbol);
            }
            let len = LEN_BASE[s] as usize + b.bits(LEN_EXTRA[s] as u32)? as usize;
            let ds = dist.decode(b)? as usize;
            if ds >= 30 {
                return Err(InflateError::BadDistance);
            }
            let d = DIST_BASE[ds] as usize + b.bits(DIST_EXTRA[ds] as u32)? as usize;
            if d > out.len() {
                return Err(InflateError::BadDistance);
            }
            let start = out.len() - d;
            for k in 0..len {
                let v = out[start + k];
                out.push(v);
            }
            if out.len() > limit {
                return Err(InflateError::TooLarge);
            }
        }
    }
}

/// Decompress a zlib stream (2-byte header, DEFLATE data, Adler-32).
pub fn zlib_decompress(data: &[u8], limit: usize) -> Result<Vec<u8>, InflateError> {
    if data.len() < 2 || data[0] & 0x0f != 8 || ((data[0] as u16) << 8 | data[1] as u16) % 31 != 0 || data[1] & 0x20 != 0 {
        return Err(InflateError::BadZlibHeader);
    }
    inflate(&data[2..], limit)
}
