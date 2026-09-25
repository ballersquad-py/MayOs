//! CAVLC residual decoding (9.2).

use alloc::vec::Vec;

use crate::bits::BitReader;
use crate::tables::*;
use crate::vlc::Vlc;

use super::{Error, Result};

pub struct CavlcTables {
    coeff_token: Vec<Vlc>, // 4 tables by nC range
    chroma_dc_token: Vlc,
    total_zeros: Vec<Vlc>, // by total_coeff - 1
    chroma_dc_total_zeros: Vec<Vlc>,
    run: Vec<Vlc>, // by min(zerosLeft, 7) - 1
}

impl CavlcTables {
    pub fn new() -> CavlcTables {
        let mut coeff_token = Vec::new();
        for t in 0..4 {
            let mut codes = Vec::new();
            for tc in 0..=16usize {
                for t1 in 0..4usize {
                    let i = t * 68 + tc * 4 + t1;
                    let len = COEFF_TOKEN_LEN[i];
                    if len > 0 && t1 <= tc {
                        codes.push((COEFF_TOKEN_BITS[i] as u32, len, (tc * 4 + t1) as i32));
                    }
                }
            }
            coeff_token.push(Vlc::new(&codes));
        }
        let mut codes = Vec::new();
        for tc in 0..=4usize {
            for t1 in 0..4usize {
                let i = tc * 4 + t1;
                if CHROMA_DC_COEFF_TOKEN_LEN[i] > 0 && t1 <= tc {
                    codes.push((CHROMA_DC_COEFF_TOKEN_BITS[i] as u32, CHROMA_DC_COEFF_TOKEN_LEN[i], (tc * 4 + t1) as i32));
                }
            }
        }
        let chroma_dc_token = Vlc::new(&codes);
        let mut total_zeros = Vec::new();
        for tc in 0..15usize {
            let mut codes = Vec::new();
            for tz in 0..16usize {
                let i = tc * 16 + tz;
                if TOTAL_ZEROS_LEN[i] > 0 {
                    codes.push((TOTAL_ZEROS_BITS[i] as u32, TOTAL_ZEROS_LEN[i], tz as i32));
                }
            }
            total_zeros.push(Vlc::new(&codes));
        }
        let mut chroma_dc_total_zeros = Vec::new();
        for tc in 0..3usize {
            let mut codes = Vec::new();
            for tz in 0..4usize {
                let i = tc * 4 + tz;
                if CHROMA_DC_TOTAL_ZEROS_LEN[i] > 0 {
                    codes.push((CHROMA_DC_TOTAL_ZEROS_BITS[i] as u32, CHROMA_DC_TOTAL_ZEROS_LEN[i], tz as i32));
                }
            }
            chroma_dc_total_zeros.push(Vlc::new(&codes));
        }
        let mut run = Vec::new();
        for z in 0..7usize {
            let mut codes = Vec::new();
            for r in 0..16usize {
                let i = z * 16 + r;
                if RUN_LEN[i] > 0 {
                    codes.push((RUN_BITS[i] as u32, RUN_LEN[i], r as i32));
                }
            }
            run.push(Vlc::new(&codes));
        }
        CavlcTables { coeff_token, chroma_dc_token, total_zeros, chroma_dc_total_zeros, run }
    }

    /// residual_block_cavlc: decodes up to `max` coefficients into
    /// `out[start..]` in scan order. `nc` < 0 selects the chroma DC table.
    /// Returns TotalCoeff.
    pub fn residual(&self, br: &mut BitReader, nc: i32, start: usize, max: usize, out: &mut [i32]) -> Result<u32> {
        let tok = if nc < 0 {
            self.chroma_dc_token.read(br)
        } else {
            let t = if nc < 2 { 0 } else if nc < 4 { 1 } else if nc < 8 { 2 } else { 3 };
            self.coeff_token[t].read(br)
        }
        .ok_or(Error::Invalid("coeff_token"))?;
        let total = (tok / 4) as usize;
        let t1 = (tok % 4) as usize;
        if total == 0 {
            return Ok(0);
        }
        if total > max {
            return Err(Error::Invalid("total_coeff"));
        }
        let mut level = [0i32; 16];
        let mut suffix_len = if total > 10 && t1 < 3 { 1 } else { 0 };
        for i in 0..total {
            if i < t1 {
                level[i] = if br.bit() == 1 { -1 } else { 1 };
                continue;
            }
            let mut prefix = 0u32;
            while br.bit() == 0 {
                prefix += 1;
                if prefix > 32 {
                    return Err(Error::Invalid("level_prefix"));
                }
            }
            let mut code = (prefix.min(15) << suffix_len) as i32;
            if suffix_len > 0 || prefix >= 14 {
                let size = if prefix == 14 && suffix_len == 0 {
                    4
                } else if prefix >= 15 {
                    prefix - 3
                } else {
                    suffix_len
                };
                if size > 0 {
                    code += br.bits(size) as i32;
                }
            }
            if prefix >= 15 && suffix_len == 0 {
                code += 15;
            }
            if prefix >= 16 {
                code += (1 << (prefix - 3)) - 4096;
            }
            if i == t1 && t1 < 3 {
                code += 2;
            }
            level[i] = if code & 1 == 0 { (code + 2) >> 1 } else { (-code - 1) >> 1 };
            if suffix_len == 0 {
                suffix_len = 1;
            }
            if level[i].abs() > (3 << (suffix_len - 1)) && suffix_len < 6 {
                suffix_len += 1;
            }
        }
        let mut zeros_left = if total < max {
            if nc < 0 {
                self.chroma_dc_total_zeros[total - 1].read(br)
            } else {
                self.total_zeros[total - 1].read(br)
            }
            .ok_or(Error::Invalid("total_zeros"))? as usize
        } else {
            0
        };
        if total + zeros_left > max {
            return Err(Error::Invalid("total_zeros"));
        }
        let mut run = [0usize; 16];
        for i in 0..total - 1 {
            if zeros_left > 0 {
                let r = self.run[zeros_left.min(7) - 1].read(br).ok_or(Error::Invalid("run_before"))? as usize;
                if r > zeros_left {
                    return Err(Error::Invalid("run_before"));
                }
                run[i] = r;
                zeros_left -= r;
            }
        }
        run[total - 1] = zeros_left;
        let mut pos: isize = -1;
        for i in (0..total).rev() {
            pos += run[i] as isize + 1;
            out[start + pos as usize] = level[i];
        }
        Ok(total as u32)
    }
}
