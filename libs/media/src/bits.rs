//! MSB-first bit reader with Exp-Golomb codes.

use alloc::vec::Vec;

#[derive(Clone)]
pub struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> BitReader<'a> {
    pub fn new(data: &'a [u8]) -> BitReader<'a> {
        BitReader { data, pos: 0 }
    }

    #[inline]
    pub fn position(&self) -> usize {
        self.pos
    }

    #[inline]
    pub fn set_position(&mut self, pos: usize) {
        self.pos = pos;
    }

    #[inline]
    pub fn bits_left(&self) -> isize {
        self.data.len() as isize * 8 - self.pos as isize
    }

    pub fn data(&self) -> &'a [u8] {
        self.data
    }

    /// Next 32 bits, zero-padded past the end.
    #[inline]
    pub fn peek32(&self) -> u32 {
        let byte = self.pos >> 3;
        let mut v: u64 = 0;
        for i in 0..5 {
            v = (v << 8) | *self.data.get(byte + i).unwrap_or(&0) as u64;
        }
        ((v << (self.pos & 7)) >> 8) as u32
    }

    #[inline]
    pub fn peek(&self, n: u32) -> u32 {
        if n == 0 { 0 } else { self.peek32() >> (32 - n) }
    }

    #[inline]
    pub fn skip(&mut self, n: u32) {
        self.pos += n as usize;
    }

    #[inline]
    pub fn bit(&mut self) -> u32 {
        let byte = *self.data.get(self.pos >> 3).unwrap_or(&0);
        let b = (byte >> (7 - (self.pos & 7))) & 1;
        self.pos += 1;
        b as u32
    }

    #[inline]
    pub fn flag(&mut self) -> bool {
        self.bit() != 0
    }

    /// Read up to 32 bits.
    #[inline]
    pub fn bits(&mut self, n: u32) -> u32 {
        if n == 0 {
            return 0;
        }
        let v = self.peek(n);
        self.pos += n as usize;
        v
    }

    pub fn bits64(&mut self, n: u32) -> u64 {
        if n <= 32 {
            self.bits(n) as u64
        } else {
            let hi = self.bits(n - 32) as u64;
            (hi << 32) | self.bits(32) as u64
        }
    }

    /// Unsigned Exp-Golomb.
    pub fn ue(&mut self) -> u32 {
        let mut zeros = 0;
        while self.bit() == 0 {
            zeros += 1;
            if zeros > 31 {
                return u32::MAX;
            }
        }
        if zeros == 0 {
            return 0;
        }
        ((1u64 << zeros) - 1 + self.bits64(zeros) as u64) as u32
    }

    /// Signed Exp-Golomb.
    pub fn se(&mut self) -> i32 {
        let k = self.ue() as i64;
        if k & 1 == 1 { ((k + 1) / 2) as i32 } else { -(k / 2) as i32 }
    }

    pub fn align(&mut self) {
        self.pos = (self.pos + 7) & !7;
    }

    pub fn byte_aligned(&self) -> bool {
        self.pos & 7 == 0
    }

    /// True if there is more data before the RBSP trailing bits.
    pub fn more_rbsp_data(&self) -> bool {
        let mut end = self.data.len();
        while end > 0 && self.data[end - 1] == 0 {
            end -= 1;
        }
        if end == 0 {
            return false;
        }
        let last = self.data[end - 1];
        let stop_bit = (end - 1) * 8 + (7 - last.trailing_zeros() as usize);
        self.pos < stop_bit
    }
}

/// Remove emulation prevention bytes (00 00 03 -> 00 00).
pub fn unescape(nal: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(nal.len());
    let mut zeros = 0;
    for &b in nal {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        if b == 0 { zeros += 1 } else { zeros = 0 }
        out.push(b);
    }
    out
}
