//! Table-driven decoding of variable-length (Huffman) codes.

use alloc::vec;
use alloc::vec::Vec;

use crate::bits::BitReader;

const ROOT_BITS: u32 = 9;

#[derive(Clone, Copy, Default)]
struct Entry {
    /// Symbol, or index of a sub-table when `len` is negative.
    sym: i32,
    /// Code length in bits (>0), -bits of the sub-table (<0), or 0 = invalid.
    len: i8,
}

/// A multi-level lookup table built from (code, length, symbol) triples.
pub struct Vlc {
    tables: Vec<(u32, Vec<Entry>)>,
}

impl Vlc {
    /// Build from explicit codes. Codes with length 0 are skipped.
    pub fn new(codes: &[(u32, u8, i32)]) -> Vlc {
        let mut v = Vlc { tables: Vec::new() };
        v.build(codes, 0, 0, ROOT_BITS);
        v
    }

    /// Build from code lengths listed in canonical order: each code is the
    /// previous one plus one, left-aligned to its length.
    pub fn from_lengths(lens: &[u8], syms: &[i32]) -> Vlc {
        let mut codes = Vec::with_capacity(lens.len());
        let mut code: u64 = 0;
        for (i, &l) in lens.iter().enumerate() {
            if l == 0 {
                continue;
            }
            codes.push(((code >> (32 - l as u32)) as u32, l, syms[i]));
            code += 1u64 << (32 - l as u32);
        }
        Vlc::new(&codes)
    }

    fn build(&mut self, codes: &[(u32, u8, i32)], prefix: u32, prefix_len: u32, bits: u32) -> usize {
        let idx = self.tables.len();
        self.tables.push((bits, vec![Entry::default(); 1 << bits]));
        let mut subs: Vec<(u32, u32)> = Vec::new(); // (index, max extra length)
        for &(code, len, sym) in codes {
            let len = len as u32;
            if len <= prefix_len || (code >> (len - prefix_len)) != prefix || len == 0 {
                continue;
            }
            let rest = len - prefix_len;
            let rest_code = if rest >= 32 { code } else { code & ((1u32 << rest) - 1) };
            if rest <= bits {
                let base = (rest_code << (bits - rest)) as usize;
                for j in 0..(1usize << (bits - rest)) {
                    self.tables[idx].1[base + j] = Entry { sym, len: rest as i8 };
                }
            } else {
                let top = rest_code >> (rest - bits);
                match subs.iter_mut().find(|s| s.0 == top) {
                    Some(s) => s.1 = s.1.max(rest - bits),
                    None => subs.push((top, rest - bits)),
                }
            }
        }
        for (top, extra) in subs {
            let sub_bits = extra.min(ROOT_BITS);
            let sub = self.build(codes, (prefix << bits) | top, prefix_len + bits, sub_bits);
            self.tables[idx].1[top as usize] = Entry { sym: sub as i32, len: -(bits as i8) };
        }
        idx
    }

    /// Decode one symbol; None for an invalid code.
    #[inline]
    pub fn read(&self, br: &mut BitReader) -> Option<i32> {
        let mut t = 0usize;
        loop {
            let (bits, ref table) = self.tables[t];
            let e = table[br.peek(bits) as usize];
            if e.len > 0 {
                br.skip(e.len as u32);
                return Some(e.sym);
            }
            if e.len == 0 {
                return None;
            }
            br.skip(bits);
            t = e.sym as usize;
        }
    }
}
