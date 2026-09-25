//! CABAC arithmetic decoding engine and binarizations (9.3).

use crate::tables::{
    CABAC_INIT, CABAC_RANGE_LPS, CABAC_TRANS_LPS, CABAC_TRANS_MPS, LAST_COEFF_OFFSET_8X8, SIG_COEFF_OFFSET_8X8,
};

pub const NUM_CTX: usize = 460;

pub struct Cabac<'a> {
    data: &'a [u8],
    /// Next byte to load.
    pos: usize,
    range: u32,
    /// codIOffset followed by `bits` prefetched bits.
    value: u64,
    bits: u32,
    /// (pStateIdx << 1) | valMPS
    ctx: [u8; NUM_CTX],
}

/// Context index offsets for residual blocks by ctxBlockCat (0..=5).
const CBF_BASE: [usize; 5] = [85, 89, 93, 97, 101];
const SIG_BASE: [usize; 6] = [105, 120, 134, 149, 152, 402];
const LAST_BASE: [usize; 6] = [166, 181, 195, 210, 213, 417];
const ABS_BASE: [usize; 6] = [227, 237, 247, 257, 266, 426];

impl<'a> Cabac<'a> {
    /// `init_idc` is 0 for I slices, else cabac_init_idc + 1.
    pub fn new(data: &'a [u8], bit_pos: usize, init_idc: usize, qp: i32) -> Cabac<'a> {
        let mut c = Cabac { data, pos: bit_pos.div_ceil(8), range: 510, value: 0, bits: 0, ctx: [0; NUM_CTX] };
        let qp = qp.clamp(0, 51);
        let table = &CABAC_INIT[init_idc * NUM_CTX * 2..][..NUM_CTX * 2];
        for i in 0..NUM_CTX {
            let (m, n) = (table[i * 2] as i32, table[i * 2 + 1] as i32);
            let pre = (((m * qp) >> 4) + n).clamp(1, 126);
            c.ctx[i] = if pre <= 63 { ((63 - pre) << 1) as u8 } else { (((pre - 64) << 1) | 1) as u8 };
        }
        c.init_engine();
        c
    }

    /// (Re)start the arithmetic decoder at the current byte position.
    pub fn init_engine(&mut self) {
        self.range = 510;
        self.value = 0;
        self.bits = 0;
        self.refill();
        // Keep 9 bits as codIOffset, the rest prefetched.
        self.bits -= 9;
    }

    #[inline]
    fn refill(&mut self) {
        while self.bits <= 48 {
            let b = if self.pos < self.data.len() { self.data[self.pos] } else { 0 };
            self.pos += 1;
            self.value = (self.value << 8) | b as u64;
            self.bits += 8;
        }
    }

    /// Byte-aligned position just after the bits consumed so far (for I_PCM).
    pub fn pcm_start(&self) -> usize {
        (self.pos * 8 - self.bits as usize).div_ceil(8)
    }

    pub fn data(&self) -> &'a [u8] {
        self.data
    }

    /// Continue after `n` bytes of PCM samples starting at `start`.
    pub fn restart_at(&mut self, start: usize) {
        self.pos = start;
        self.init_engine();
    }

    pub fn overrun(&self) -> bool {
        self.pos > self.data.len() + 16
    }

    #[inline]
    pub fn decision(&mut self, ctx: usize) -> u32 {
        let s = self.ctx[ctx];
        let state = (s >> 1) as usize;
        let mps = (s & 1) as u32;
        let lps = CABAC_RANGE_LPS[state * 4 + ((self.range >> 6) & 3) as usize] as u32;
        self.range -= lps;
        let bin;
        let scaled = (self.range as u64) << self.bits;
        if self.value >= scaled {
            bin = mps ^ 1;
            self.value -= scaled;
            self.range = lps;
            let mut ns = CABAC_TRANS_LPS[state] << 1;
            ns |= if state == 0 { (mps ^ 1) as u8 } else { mps as u8 };
            self.ctx[ctx] = ns;
        } else {
            bin = mps;
            self.ctx[ctx] = (CABAC_TRANS_MPS[state] << 1) | mps as u8;
        }
        if self.range < 256 {
            let shift = self.range.leading_zeros() - 23;
            self.range <<= shift;
            self.bits -= shift;
            if self.bits < 16 {
                self.refill();
            }
        }
        bin
    }

    #[inline]
    pub fn bypass(&mut self) -> u32 {
        self.bits -= 1;
        let scaled = (self.range as u64) << self.bits;
        let r = if self.value >= scaled {
            self.value -= scaled;
            1
        } else {
            0
        };
        if self.bits < 16 {
            self.refill();
        }
        r
    }

    pub fn terminate(&mut self) -> u32 {
        self.range -= 2;
        let scaled = (self.range as u64) << self.bits;
        if self.value >= scaled {
            1
        } else {
            if self.range < 256 {
                let shift = self.range.leading_zeros() - 23;
                self.range <<= shift;
                self.bits -= shift;
                if self.bits < 16 {
                    self.refill();
                }
            }
            0
        }
    }

    fn exp_golomb_bypass(&mut self, mut k: u32) -> u32 {
        let mut v = 0u32;
        while self.bypass() == 1 {
            v += 1 << k;
            k += 1;
            if k > 24 {
                return v;
            }
        }
        while k > 0 {
            k -= 1;
            v += self.bypass() << k;
        }
        v
    }

    // -----------------------------------------------------------------
    // Syntax elements
    // -----------------------------------------------------------------

    pub fn mb_skip(&mut self, b_slice: bool, inc: usize) -> bool {
        self.decision(if b_slice { 24 } else { 11 } + inc) == 1
    }

    /// mb_type for I macroblocks: 0 = I_NxN, 1..=24 = I_16x16, 25 = I_PCM.
    /// `prefix_base` is 3 in I slices (with neighbour `inc`), 17 in P, 32 in B.
    pub fn mb_type_intra(&mut self, prefix_base: usize, inc: usize) -> u32 {
        let islice = prefix_base == 3;
        if self.decision(prefix_base + if islice { inc } else { 0 }) == 0 {
            return 0;
        }
        if self.terminate() == 1 {
            return 25;
        }
        let (luma, c1, c2, p1, p2) = if islice { (6, 7, 8, 9, 10) } else { (prefix_base + 1, prefix_base + 2, prefix_base + 2, prefix_base + 3, prefix_base + 3) };
        let mut t = 1 + 12 * self.decision(luma);
        if self.decision(c1) == 1 {
            t += 4 + 4 * self.decision(c2);
        }
        t += 2 * self.decision(p1);
        t += self.decision(p2);
        t
    }

    /// P mb_type: 0..=3 inter types (P_8x8ref0 never occurs with CABAC), 5.. intra.
    pub fn mb_type_p(&mut self) -> u32 {
        if self.decision(14) == 0 {
            if self.decision(15) == 0 { 3 * self.decision(16) } else { 2 - self.decision(17) }
        } else {
            5 + self.mb_type_intra(17, 0)
        }
    }

    pub fn mb_type_b(&mut self, inc: usize) -> u32 {
        if self.decision(27 + inc) == 0 {
            return 0;
        }
        if self.decision(27 + 3) == 0 {
            return 1 + self.decision(27 + 5);
        }
        let mut bits = self.decision(27 + 4) << 3;
        bits |= self.decision(27 + 5) << 2;
        bits |= self.decision(27 + 5) << 1;
        bits |= self.decision(27 + 5);
        if bits < 8 {
            bits + 3
        } else if bits == 13 {
            23 + self.mb_type_intra(32, 0)
        } else if bits == 14 {
            11
        } else if bits == 15 {
            22
        } else {
            bits = (bits << 1) | self.decision(27 + 5);
            bits - 4
        }
    }

    pub fn sub_mb_type_p(&mut self) -> u32 {
        if self.decision(21) == 1 {
            return 0;
        }
        if self.decision(22) == 0 {
            return 1;
        }
        if self.decision(23) == 1 { 2 } else { 3 }
    }

    pub fn sub_mb_type_b(&mut self) -> u32 {
        if self.decision(36) == 0 {
            return 0;
        }
        if self.decision(37) == 0 {
            return 1 + self.decision(39);
        }
        let mut t = 3;
        if self.decision(38) == 1 {
            if self.decision(39) == 1 {
                return 11 + self.decision(39);
            }
            t += 4;
        }
        t += 2 * self.decision(39);
        t += self.decision(39);
        t
    }

    pub fn ref_idx(&mut self, inc: usize) -> u32 {
        let mut v = 0;
        let mut ctx = 54 + inc;
        while self.decision(ctx) == 1 {
            v += 1;
            ctx = if v == 1 { 54 + 4 } else { 54 + 5 };
            if v > 32 {
                break;
            }
        }
        v
    }

    /// `comp` 0 = horizontal, 1 = vertical; `sum` = |mvdA| + |mvdB|.
    pub fn mvd(&mut self, comp: usize, sum: u32) -> i32 {
        let base = if comp == 0 { 40 } else { 47 };
        let inc = if sum < 3 { 0 } else if sum > 32 { 2 } else { 1 };
        if self.decision(base + inc) == 0 {
            return 0;
        }
        let mut prefix = 1;
        let mut ctx = base + 3;
        while prefix < 9 && self.decision(ctx) == 1 {
            prefix += 1;
            if ctx < base + 6 {
                ctx += 1;
            }
        }
        let mut v = prefix;
        if prefix >= 9 {
            v += self.exp_golomb_bypass(3);
        }
        if self.bypass() == 1 { -(v as i32) } else { v as i32 }
    }

    /// prev_intra_pred_mode_flag + rem_intra_pred_mode: None = use predicted.
    pub fn intra_mode(&mut self) -> Option<u8> {
        if self.decision(68) == 1 {
            return None;
        }
        let mut v = self.decision(69);
        v |= self.decision(69) << 1;
        v |= self.decision(69) << 2;
        Some(v as u8)
    }

    pub fn chroma_pred_mode(&mut self, inc: usize) -> u8 {
        if self.decision(64 + inc) == 0 {
            return 0;
        }
        if self.decision(64 + 3) == 0 {
            return 1;
        }
        if self.decision(64 + 3) == 0 { 2 } else { 3 }
    }

    pub fn qp_delta(&mut self, prev_nonzero: bool) -> i32 {
        if self.decision(60 + prev_nonzero as usize) == 0 {
            return 0;
        }
        let mut k = 1;
        let mut ctx = 60 + 2;
        while self.decision(ctx) == 1 {
            k += 1;
            ctx = 60 + 3;
            if k > 104 {
                break;
            }
        }
        if k & 1 == 1 { (k + 1) / 2 } else { -(k / 2) }
    }

    pub fn transform_8x8(&mut self, inc: usize) -> bool {
        self.decision(399 + inc) == 1
    }

    pub fn coded_block_flag(&mut self, cat: usize, inc: usize) -> bool {
        self.decision(CBF_BASE[cat] + inc) == 1
    }

    /// Decode significance map and levels of one block (ctxBlockCat `cat`)
    /// with `max` coefficients into `out` (in scan order). Returns the
    /// number of non-zero coefficients.
    pub fn residual(&mut self, cat: usize, max: usize, out: &mut [i32]) -> u32 {
        let (sig, last, abs) = (SIG_BASE[cat], LAST_BASE[cat], ABS_BASE[cat]);
        let mut index = [0u8; 64];
        let mut n = 0usize;
        let mut i = 0usize;
        let mut ended = false;
        while i < max - 1 {
            let (si, li) = match cat {
                3 => (i.min(2), i.min(2)),
                5 => (SIG_COEFF_OFFSET_8X8[i] as usize, LAST_COEFF_OFFSET_8X8[i] as usize),
                _ => (i, i),
            };
            if self.decision(sig + si) == 1 {
                index[n] = i as u8;
                n += 1;
                if self.decision(last + li) == 1 {
                    ended = true;
                    break;
                }
            }
            i += 1;
        }
        if !ended {
            index[n] = (max - 1) as u8;
            n += 1;
        }
        let mut eq1 = 0u32;
        let mut gt1 = 0u32;
        let gt1_cap = if cat == 3 { 3 } else { 4 };
        for k in (0..n).rev() {
            let inc0 = if gt1 != 0 { 0 } else { (1 + eq1).min(4) } as usize;
            let level;
            if self.decision(abs + inc0) == 0 {
                level = 1;
                eq1 += 1;
            } else {
                let ctx = abs + 5 + gt1.min(gt1_cap) as usize;
                let mut p = 1;
                while p < 14 && self.decision(ctx) == 1 {
                    p += 1;
                }
                let mut m1 = p;
                if p >= 14 {
                    m1 += self.exp_golomb_bypass(0);
                }
                level = m1 as i32 + 1;
                gt1 += 1;
            }
            out[index[k] as usize] = if self.bypass() == 1 { -level } else { level };
        }
        n as u32
    }
}
