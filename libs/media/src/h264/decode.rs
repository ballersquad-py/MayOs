//! Slice data and macroblock layer: parsing (CAVLC and CABAC), motion vector
//! prediction, direct modes, and reconstruction.

use alloc::vec::Vec;

use crate::bits::BitReader;
use crate::tables::{GOLOMB_TO_INTER_CBP, GOLOMB_TO_INTRA_CBP};

use super::cabac::Cabac;
use super::cavlc::CavlcTables;
use super::intra::{self, Avail};
use super::inter;
use super::ps::{Pps, Sps};
use super::slice::{SliceHeader, SliceType};
use super::transform::{self, clip_u8};
use super::types::*;
use super::{Error, Result, BLK, ZIGZAG4, ZIGZAG8};

pub enum Ent<'a> {
    Cabac(Cabac<'a>),
    Cavlc(BitReader<'a>),
}

#[inline]
fn b8_of(blk: usize) -> usize {
    (blk / 8) * 2 + (blk % 4) / 2
}

/// Rectangle in 4x4 units within the macroblock.
#[derive(Clone, Copy)]
struct Part {
    x: usize,
    y: usize,
    w: usize,
    h: usize,
}

pub struct SliceDec<'a> {
    pub sps: &'a Sps,
    pub pps: &'a Pps,
    pub hdr: &'a SliceHeader,
    pub pic: &'a mut CurPic,
    pub lists: &'a [Vec<RefPic>; 2],
    pub cavlc: &'a CavlcTables,
    pub slice_num: u32,
    ls4: [[[i32; 16]; 6]; 6],
    ls8: [[[i32; 64]; 6]; 2],
    qp: i32,
    last_dqp: bool,
    addr: usize,
    mb_x: usize,
    mb_y: usize,
    implicit: Vec<(i32, i32)>,
    coef: [[i32; 16]; 16],
    coef8: [[i32; 64]; 4],
    dc: [i32; 16],
    cdc: [[i32; 4]; 2],
    cac: [[i32; 16]; 8],
    mvd: [[[i32; 2]; 16]; 2],
    spatial: Option<([i8; 2], [[i16; 2]; 2])>,
}

impl<'a> SliceDec<'a> {
    pub fn new(
        sps: &'a Sps,
        pps: &'a Pps,
        hdr: &'a SliceHeader,
        pic: &'a mut CurPic,
        lists: &'a [Vec<RefPic>; 2],
        cavlc: &'a CavlcTables,
        slice_num: u32,
    ) -> SliceDec<'a> {
        let mut ls4 = [[[0i32; 16]; 6]; 6];
        for (i, l) in ls4.iter_mut().enumerate() {
            *l = transform::level_scale4(&pps.scaling4[i]);
        }
        let ls8 = [transform::level_scale8(&pps.scaling8[0]), transform::level_scale8(&pps.scaling8[1])];
        let mut implicit = Vec::new();
        if hdr.slice_type == SliceType::B && pps.weighted_bipred_idc == 2 {
            let cur = pic.poc;
            for r0 in lists[0].iter() {
                for r1 in lists[1].iter() {
                    let tb = (cur - r0.poc()).clamp(-128, 127);
                    let td = (r1.poc() - r0.poc()).clamp(-128, 127);
                    let w = if td == 0 || r0.long_term || r1.long_term {
                        (32, 32)
                    } else {
                        let tx = (16384 + (td / 2).abs()) / td;
                        let dsf = ((tb * tx + 32) >> 6).clamp(-1024, 1023);
                        if (dsf >> 2) < -64 || (dsf >> 2) > 128 { (32, 32) } else { (64 - (dsf >> 2), dsf >> 2) }
                    };
                    implicit.push(w);
                }
            }
        }
        SliceDec {
            sps,
            pps,
            hdr,
            pic,
            lists,
            cavlc,
            slice_num,
            ls4,
            ls8,
            qp: hdr.qp,
            last_dqp: false,
            addr: 0,
            mb_x: 0,
            mb_y: 0,
            implicit,
            coef: [[0; 16]; 16],
            coef8: [[0; 64]; 4],
            dc: [0; 16],
            cdc: [[0; 4]; 2],
            cac: [[0; 16]; 8],
            mvd: [[[0; 2]; 16]; 2],
            spatial: None,
        }
    }

    pub fn run(&mut self, rbsp: &[u8]) -> Result<()> {
        let total = self.pic.mb_w * self.pic.mb_h;
        let mut addr = self.hdr.first_mb as usize;
        if addr >= total {
            return Err(Error::Invalid("first_mb_in_slice"));
        }
        let st = self.hdr.slice_type;
        if self.pps.cabac {
            let init = if st == SliceType::I { 0 } else { self.hdr.cabac_init_idc as usize + 1 };
            let mut ent = Ent::Cabac(Cabac::new(rbsp, self.hdr.data_bit, init, self.hdr.qp));
            loop {
                self.set_addr(addr);
                let skip = if st != SliceType::I {
                    let inc = self.skip_cond(-1, 0) + self.skip_cond(0, -1);
                    match &mut ent {
                        Ent::Cabac(c) => c.mb_skip(st == SliceType::B, inc),
                        _ => unreachable!(),
                    }
                } else {
                    false
                };
                if skip {
                    self.skip_mb()?;
                } else {
                    self.macroblock(&mut ent)?;
                }
                self.pic.decoded += 1;
                addr += 1;
                let Ent::Cabac(c) = &mut ent else { unreachable!() };
                if c.terminate() == 1 {
                    break;
                }
                if addr >= total || c.overrun() {
                    return Err(Error::Invalid("slice data overrun"));
                }
            }
        } else {
            let mut br = BitReader::new(rbsp);
            br.set_position(self.hdr.data_bit);
            let mut ent = Ent::Cavlc(br);
            loop {
                if st != SliceType::I {
                    let Ent::Cavlc(br) = &mut ent else { unreachable!() };
                    let run = br.ue() as usize;
                    if run > total - addr {
                        return Err(Error::Invalid("mb_skip_run"));
                    }
                    for _ in 0..run {
                        self.set_addr(addr);
                        self.skip_mb()?;
                        self.pic.decoded += 1;
                        addr += 1;
                    }
                    let Ent::Cavlc(br) = &mut ent else { unreachable!() };
                    if run > 0 && !br.more_rbsp_data() {
                        break;
                    }
                }
                if addr >= total {
                    break;
                }
                self.set_addr(addr);
                self.macroblock(&mut ent)?;
                self.pic.decoded += 1;
                addr += 1;
                let Ent::Cavlc(br) = &mut ent else { unreachable!() };
                if br.bits_left() < 0 {
                    return Err(Error::Invalid("slice data overrun"));
                }
                if !br.more_rbsp_data() {
                    break;
                }
            }
        }
        Ok(())
    }

    fn set_addr(&mut self, addr: usize) {
        self.addr = addr;
        self.mb_x = addr % self.pic.mb_w;
        self.mb_y = addr / self.pic.mb_w;
        self.spatial = None;
        let mut i = MbInfo::EMPTY;
        i.slice = self.slice_num;
        self.pic.info[addr] = i;
    }

    // -----------------------------------------------------------------
    // Neighbours
    // -----------------------------------------------------------------

    /// Macroblock at (dx, dy) relative to the current one, if available.
    #[inline]
    fn mb_at(&self, dx: isize, dy: isize) -> Option<usize> {
        let x = self.mb_x as isize + dx;
        let y = self.mb_y as isize + dy;
        if x < 0 || y < 0 || x >= self.pic.mb_w as isize {
            return None;
        }
        let a = y as usize * self.pic.mb_w + x as usize;
        if self.pic.info[a].slice == self.slice_num { Some(a) } else { None }
    }

    /// 4x4 luma block at (bx, by) in 4x4 units relative to the current MB.
    #[inline]
    fn nb4(&self, bx: isize, by: isize) -> Option<(usize, usize)> {
        if (0..4).contains(&bx) && (0..4).contains(&by) {
            return Some((self.addr, (by * 4 + bx) as usize));
        }
        if by >= 4 {
            return None;
        }
        let dx = if bx < 0 { -1 } else if bx >= 4 { 1 } else { 0 };
        let dy = if by < 0 { -1 } else { 0 };
        if dx == 1 && dy == 0 {
            return None;
        }
        let m = self.mb_at(dx, dy)?;
        let x = (bx + 4) % 4;
        let y = (by + 4) % 4;
        Some((m, (y * 4 + x) as usize))
    }

    /// Chroma 4x4 block (cx, cy in 2x2 units) for component `c`: nz index.
    #[inline]
    fn nb_chroma(&self, c: usize, cx: isize, cy: isize) -> Option<(usize, usize)> {
        if cx >= 0 && cy >= 0 {
            return Some((self.addr, 16 + c * 4 + (cy * 2 + cx) as usize));
        }
        let m = self.mb_at(if cx < 0 { -1 } else { 0 }, if cy < 0 { -1 } else { 0 })?;
        let x = (cx + 2) % 2;
        let y = (cy + 2) % 2;
        Some((m, 16 + c * 4 + (y * 2 + x) as usize))
    }

    fn cur(&mut self) -> &mut MbInfo {
        &mut self.pic.info[self.addr]
    }

    fn cur_intra(&self) -> bool {
        self.pic.info[self.addr].is_intra()
    }

    fn skip_cond(&self, dx: isize, dy: isize) -> usize {
        match self.mb_at(dx, dy) {
            Some(m) => !self.pic.info[m].is_skip() as usize,
            None => 0,
        }
    }

    fn intra_avail(&self, m: Option<usize>) -> bool {
        match m {
            Some(m) => !self.pps.constrained_intra_pred || self.pic.info[m].is_intra(),
            None => false,
        }
    }

    // -----------------------------------------------------------------
    // Macroblock layer
    // -----------------------------------------------------------------

    fn macroblock(&mut self, ent: &mut Ent) -> Result<()> {
        let st = self.hdr.slice_type;
        let raw = match ent {
            Ent::Cabac(c) => match st {
                SliceType::I => {
                    let inc = self.itype_cond(-1, 0) + self.itype_cond(0, -1);
                    c.mb_type_intra(3, inc)
                }
                SliceType::P => c.mb_type_p(),
                SliceType::B => {
                    let cond = |s: &Self, dx, dy| match s.mb_at(dx, dy) {
                        Some(m) => !s.pic.info[m].direct16 as usize,
                        None => 0,
                    };
                    let inc = cond(self, -1, 0) + cond(self, 0, -1);
                    c.mb_type_b(inc)
                }
            },
            Ent::Cavlc(br) => br.ue(),
        };
        match st {
            SliceType::I => self.intra_mb(ent, raw),
            SliceType::P => {
                if raw < 5 {
                    self.inter_mb(ent, raw)
                } else {
                    self.intra_mb(ent, raw - 5)
                }
            }
            SliceType::B => {
                if raw < 23 {
                    self.inter_mb(ent, raw)
                } else {
                    self.intra_mb(ent, raw - 23)
                }
            }
        }
    }

    fn itype_cond(&self, dx: isize, dy: isize) -> usize {
        match self.mb_at(dx, dy) {
            Some(m) => {
                let k = self.pic.info[m].kind;
                (k != KIND_I4 && k != KIND_I8) as usize
            }
            None => 0,
        }
    }

    fn read_t8(&mut self, ent: &mut Ent) -> bool {
        match ent {
            Ent::Cabac(c) => {
                let cond = |s: &Self, dx, dy| match s.mb_at(dx, dy) {
                    Some(m) => s.pic.info[m].t8x8 as usize,
                    None => 0,
                };
                let inc = cond(self, -1, 0) + cond(self, 0, -1);
                c.transform_8x8(inc)
            }
            Ent::Cavlc(br) => br.flag(),
        }
    }

    fn pred_mode(&self, bx: isize, by: isize) -> u8 {
        let get = |n: Option<(usize, usize)>| -> Option<u8> {
            let (m, blk) = n?;
            let inf = &self.pic.info[m];
            if !inf.is_intra() && self.pps.constrained_intra_pred {
                return None;
            }
            Some(inf.intra_modes[blk])
        };
        match (get(self.nb4(bx - 1, by)), get(self.nb4(bx, by - 1))) {
            (Some(a), Some(b)) => a.min(b),
            _ => 2,
        }
    }

    fn read_intra_mode(&mut self, ent: &mut Ent, pred: u8) -> Result<u8> {
        let rem = match ent {
            Ent::Cabac(c) => c.intra_mode(),
            Ent::Cavlc(br) => {
                if br.flag() {
                    None
                } else {
                    Some(br.bits(3) as u8)
                }
            }
        };
        Ok(match rem {
            None => pred,
            Some(r) if r < pred => r,
            Some(r) => r + 1,
        })
    }

    fn read_chroma_mode(&mut self, ent: &mut Ent) -> Result<u8> {
        let m = match ent {
            Ent::Cabac(c) => {
                let cond = |s: &Self, dx, dy| match s.mb_at(dx, dy) {
                    Some(m) => {
                        let i = &s.pic.info[m];
                        (i.is_intra() && i.kind != KIND_PCM && i.chroma_mode != 0) as usize
                    }
                    None => 0,
                };
                let inc = cond(self, -1, 0) + cond(self, 0, -1);
                c.chroma_pred_mode(inc)
            }
            Ent::Cavlc(br) => br.ue() as u8,
        };
        if m > 3 {
            return Err(Error::Invalid("intra_chroma_pred_mode"));
        }
        Ok(m)
    }

    fn read_cbp(&mut self, ent: &mut Ent, intra: bool) -> Result<u8> {
        match ent {
            Ent::Cavlc(br) => {
                let v = br.ue() as usize;
                if v > 47 {
                    return Err(Error::Invalid("coded_block_pattern"));
                }
                Ok(if intra { GOLOMB_TO_INTRA_CBP[v] } else { GOLOMB_TO_INTER_CBP[v] })
            }
            Ent::Cabac(c) => {
                let a = self.mb_at(-1, 0).map(|m| self.pic.info[m].cbp);
                let b = self.mb_at(0, -1).map(|m| self.pic.info[m].cbp);
                let mut luma = 0u8;
                for b8 in 0..4 {
                    let ca = if b8 & 1 == 1 {
                        ((luma >> (b8 - 1)) & 1 == 0) as usize
                    } else {
                        a.map(|v| ((v >> (b8 + 1)) & 1 == 0) as usize).unwrap_or(0)
                    };
                    let cb = if b8 >= 2 {
                        ((luma >> (b8 - 2)) & 1 == 0) as usize
                    } else {
                        b.map(|v| ((v >> (b8 + 2)) & 1 == 0) as usize).unwrap_or(0)
                    };
                    luma |= (c.decision(73 + ca + 2 * cb) as u8) << b8;
                }
                let ca = a.map(|v| (v >> 4 != 0) as usize).unwrap_or(0);
                let cb = b.map(|v| (v >> 4 != 0) as usize).unwrap_or(0);
                let mut chroma = 0u8;
                if c.decision(77 + ca + 2 * cb) == 1 {
                    let ca = a.map(|v| (v >> 4 == 2) as usize).unwrap_or(0);
                    let cb = b.map(|v| (v >> 4 == 2) as usize).unwrap_or(0);
                    chroma = 1 + c.decision(77 + 4 + ca + 2 * cb) as u8;
                }
                Ok(luma | (chroma << 4))
            }
        }
    }

    fn read_dqp(&mut self, ent: &mut Ent) -> Result<()> {
        let d = match ent {
            Ent::Cabac(c) => c.qp_delta(self.last_dqp),
            Ent::Cavlc(br) => br.se(),
        };
        if !(-26..=25).contains(&d) {
            return Err(Error::Invalid("mb_qp_delta"));
        }
        self.last_dqp = d != 0;
        self.qp = (self.qp + d + 52) % 52;
        Ok(())
    }

    fn intra_mb(&mut self, ent: &mut Ent, t: u32) -> Result<()> {
        if t > 25 {
            return Err(Error::Invalid("mb_type"));
        }
        if t == 25 {
            return self.pcm(ent);
        }
        let (mut kind, mut cbp, i16_mode) = if t == 0 {
            (KIND_I4, 0u8, 0u8)
        } else {
            let chroma = ((t - 1) / 4) % 3;
            let luma = if t >= 13 { 15 } else { 0 };
            (KIND_I16, (luma | (chroma << 4)) as u8, ((t - 1) % 4) as u8)
        };
        self.cur().kind = kind;
        if t == 0 {
            if self.pps.transform_8x8_mode && self.read_t8(ent) {
                kind = KIND_I8;
                self.cur().kind = kind;
                self.cur().t8x8 = true;
            }
            if kind == KIND_I8 {
                for b8 in 0..4 {
                    let (bx, by) = ((b8 % 2) * 2, (b8 / 2) * 2);
                    let pred = self.pred_mode(bx as isize, by as isize);
                    let m = self.read_intra_mode(ent, pred)?;
                    for (dx, dy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                        self.cur().intra_modes[(by + dy) * 4 + bx + dx] = m;
                    }
                }
            } else {
                for idx in 0..16 {
                    let blk = BLK[idx] as usize;
                    let pred = self.pred_mode((blk % 4) as isize, (blk / 4) as isize);
                    let m = self.read_intra_mode(ent, pred)?;
                    self.cur().intra_modes[blk] = m;
                }
            }
        }
        let cm = self.read_chroma_mode(ent)?;
        self.cur().chroma_mode = cm;
        if kind != KIND_I16 {
            cbp = self.read_cbp(ent, true)?;
        }
        self.cur().cbp = cbp;
        if cbp != 0 || kind == KIND_I16 {
            self.read_dqp(ent)?;
            self.residual(ent, kind, cbp)?;
        } else {
            self.last_dqp = false;
        }
        let qp = self.qp as u8;
        self.cur().qp = qp;
        self.recon_intra(kind, i16_mode, cm, cbp);
        Ok(())
    }

    fn pcm(&mut self, ent: &mut Ent) -> Result<()> {
        let mut samples = [0u8; 384];
        match ent {
            Ent::Cabac(c) => {
                let start = c.pcm_start();
                let d = c.data();
                if start + 384 > d.len() {
                    return Err(Error::Invalid("I_PCM truncated"));
                }
                samples.copy_from_slice(&d[start..start + 384]);
                c.restart_at(start + 384);
            }
            Ent::Cavlc(br) => {
                br.align();
                for s in samples.iter_mut() {
                    *s = br.bits(8) as u8;
                }
            }
        }
        let stride = self.pic.mb_w * 16;
        let cs = self.pic.mb_w * 8;
        for y in 0..16 {
            let o = (self.mb_y * 16 + y) * stride + self.mb_x * 16;
            self.pic.y[o..o + 16].copy_from_slice(&samples[y * 16..y * 16 + 16]);
        }
        for y in 0..8 {
            let o = (self.mb_y * 8 + y) * cs + self.mb_x * 8;
            self.pic.cb[o..o + 8].copy_from_slice(&samples[256 + y * 8..256 + y * 8 + 8]);
            self.pic.cr[o..o + 8].copy_from_slice(&samples[320 + y * 8..320 + y * 8 + 8]);
        }
        let i = self.cur();
        i.kind = KIND_PCM;
        i.qp = 0;
        i.nz = [16; 24];
        i.cbf_dc = 7;
        i.cbp = 0x2f;
        self.last_dqp = false;
        Ok(())
    }

    // -----------------------------------------------------------------
    // Residual
    // -----------------------------------------------------------------

    fn luma_nc(&self, blk: usize) -> i32 {
        let (bx, by) = ((blk % 4) as isize, (blk / 4) as isize);
        let a = self.nb4(bx - 1, by).map(|(m, b)| self.pic.info[m].nz[b] as i32);
        let b = self.nb4(bx, by - 1).map(|(m, b)| self.pic.info[m].nz[b] as i32);
        match (a, b) {
            (Some(a), Some(b)) => (a + b + 1) >> 1,
            (Some(a), None) => a,
            (None, Some(b)) => b,
            _ => 0,
        }
    }

    fn chroma_nc(&self, c: usize, b: usize) -> i32 {
        let (cx, cy) = ((b % 2) as isize, (b / 2) as isize);
        let a = self.nb_chroma(c, cx - 1, cy).map(|(m, i)| self.pic.info[m].nz[i] as i32);
        let bb = self.nb_chroma(c, cx, cy - 1).map(|(m, i)| self.pic.info[m].nz[i] as i32);
        match (a, bb) {
            (Some(a), Some(b)) => (a + b + 1) >> 1,
            (Some(a), None) => a,
            (None, Some(b)) => b,
            _ => 0,
        }
    }

    /// coded_block_flag context for a block whose neighbour flags are given.
    fn cbf_inc(&self, a: Option<bool>, b: Option<bool>) -> usize {
        let intra = self.cur_intra();
        let f = |x: Option<bool>| x.unwrap_or(intra) as usize;
        f(a) + 2 * f(b)
    }

    fn luma_cbf_inc(&self, blk: usize) -> usize {
        let (bx, by) = ((blk % 4) as isize, (blk / 4) as isize);
        let a = self.nb4(bx - 1, by).map(|(m, b)| self.pic.info[m].nz[b] > 0);
        let b = self.nb4(bx, by - 1).map(|(m, b)| self.pic.info[m].nz[b] > 0);
        self.cbf_inc(a, b)
    }

    /// Parse one residual block. Returns the coefficient count; `scan`
    /// receives the coefficients in scan order starting at index 0.
    #[allow(clippy::too_many_arguments)]
    fn block(&mut self, ent: &mut Ent, cat: usize, max: usize, nc: i32, cbf_inc: usize, scan: &mut [i32; 64]) -> Result<u32> {
        match ent {
            Ent::Cabac(c) => {
                if cat != 5 && !c.coded_block_flag(cat, cbf_inc) {
                    return Ok(0);
                }
                Ok(c.residual(cat, max, scan))
            }
            Ent::Cavlc(br) => self.cavlc.residual(br, nc, 0, max, scan),
        }
    }

    fn residual(&mut self, ent: &mut Ent, kind: u8, cbp: u8) -> Result<()> {
        let t8 = self.pic.info[self.addr].t8x8;
        let is_cabac = matches!(ent, Ent::Cabac(_));
        let mut scan;
        if kind == KIND_I16 {
            let a = self.mb_at(-1, 0).map(|m| self.pic.info[m].cbf_dc & 1 != 0);
            let b = self.mb_at(0, -1).map(|m| self.pic.info[m].cbf_dc & 1 != 0);
            let inc = self.cbf_inc(a, b);
            let nc = self.luma_nc(0);
            scan = [0; 64];
            let n = self.block(ent, 0, 16, nc, inc, &mut scan)?;
            self.dc = [0; 16];
            for i in 0..16 {
                self.dc[ZIGZAG4[i] as usize] = scan[i];
            }
            if n > 0 {
                self.cur().cbf_dc |= 1;
            }
        }
        for b8 in 0..4 {
            if cbp & (1 << b8) == 0 {
                continue;
            }
            if t8 {
                self.coef8[b8] = [0; 64];
                if is_cabac {
                    scan = [0; 64];
                    let n = self.block(ent, 5, 64, 0, 0, &mut scan)?;
                    for i in 0..64 {
                        self.coef8[b8][ZIGZAG8[i] as usize] = scan[i];
                    }
                    for k in 0..4 {
                        let blk = BLK[b8 * 4 + k] as usize;
                        self.cur().nz[blk] = n as u8;
                    }
                } else {
                    for k in 0..4 {
                        let blk = BLK[b8 * 4 + k] as usize;
                        let nc = self.luma_nc(blk);
                        scan = [0; 64];
                        let n = self.block(ent, 2, 16, nc, 0, &mut scan)?;
                        for i in 0..16 {
                            self.coef8[b8][ZIGZAG8[4 * i + k] as usize] = scan[i];
                        }
                        self.cur().nz[blk] = n as u8;
                    }
                }
            } else {
                for k in 0..4 {
                    let blk = BLK[b8 * 4 + k] as usize;
                    let nc = if is_cabac { 0 } else { self.luma_nc(blk) };
                    let inc = if is_cabac { self.luma_cbf_inc(blk) } else { 0 };
                    scan = [0; 64];
                    self.coef[blk] = [0; 16];
                    let n = if kind == KIND_I16 {
                        let n = self.block(ent, 1, 15, nc, inc, &mut scan)?;
                        for i in 0..15 {
                            self.coef[blk][ZIGZAG4[i + 1] as usize] = scan[i];
                        }
                        n
                    } else {
                        let n = self.block(ent, 2, 16, nc, inc, &mut scan)?;
                        for i in 0..16 {
                            self.coef[blk][ZIGZAG4[i] as usize] = scan[i];
                        }
                        n
                    };
                    self.cur().nz[blk] = n as u8;
                }
            }
        }
        let chroma = cbp >> 4;
        if chroma != 0 {
            for c in 0..2 {
                let bit = 2 << c;
                let a = self.mb_at(-1, 0).map(|m| self.pic.info[m].cbf_dc & bit != 0);
                let b = self.mb_at(0, -1).map(|m| self.pic.info[m].cbf_dc & bit != 0);
                let inc = self.cbf_inc(a, b);
                scan = [0; 64];
                let n = self.block(ent, 3, 4, -1, inc, &mut scan)?;
                self.cdc[c].copy_from_slice(&scan[..4]);
                if n > 0 {
                    self.cur().cbf_dc |= bit;
                }
            }
        }
        if chroma == 2 {
            for c in 0..2 {
                for b in 0..4 {
                    let (cx, cy) = ((b % 2) as isize, (b / 2) as isize);
                    let nc = if is_cabac { 0 } else { self.chroma_nc(c, b) };
                    let inc = if is_cabac {
                        let a = self.nb_chroma(c, cx - 1, cy).map(|(m, i)| self.pic.info[m].nz[i] > 0);
                        let bb = self.nb_chroma(c, cx, cy - 1).map(|(m, i)| self.pic.info[m].nz[i] > 0);
                        self.cbf_inc(a, bb)
                    } else {
                        0
                    };
                    scan = [0; 64];
                    let n = self.block(ent, 4, 15, nc, inc, &mut scan)?;
                    let dst = &mut self.cac[c * 4 + b];
                    *dst = [0; 16];
                    for i in 0..15 {
                        dst[ZIGZAG4[i + 1] as usize] = scan[i];
                    }
                    self.pic.info[self.addr].nz[16 + c * 4 + b] = n as u8;
                }
            }
        }
        Ok(())
    }

    // -----------------------------------------------------------------
    // Intra reconstruction
    // -----------------------------------------------------------------

    fn luma_avail(&self, bx: usize, by: usize, size: usize) -> Avail {
        // (bx, by) in 4x4 units, block size in 4x4 units (1 or 2).
        let a = self.intra_avail(self.mb_at(-1, 0));
        let b = self.intra_avail(self.mb_at(0, -1));
        let c = self.intra_avail(self.mb_at(1, -1));
        let d = self.intra_avail(self.mb_at(-1, -1));
        let left = bx > 0 || a;
        let top = by > 0 || b;
        let topleft = match (bx > 0, by > 0) {
            (true, true) => true,
            (false, true) => a,
            (true, false) => b,
            (false, false) => d,
        };
        let tx = bx + size;
        let topright = if by == 0 {
            if tx < 4 { b } else { c }
        } else if tx >= 4 {
            false
        } else {
            let here = BLK[by * 4 + bx];
            let there = BLK[(by - 1) * 4 + tx];
            there < here
        };
        Avail { left, top, topright, topleft }
    }

    fn recon_intra(&mut self, kind: u8, i16_mode: u8, cm: u8, cbp: u8) {
        let stride = self.pic.mb_w * 16;
        let x0 = self.mb_x * 16;
        let y0 = self.mb_y * 16;
        let qp = self.qp;
        match kind {
            KIND_I4 => {
                for idx in 0..16 {
                    let blk = BLK[idx] as usize;
                    let (bx, by) = (blk % 4, blk / 4);
                    let av = self.luma_avail(bx, by, 1);
                    let mode = self.pic.info[self.addr].intra_modes[blk];
                    intra::pred_nxn(&mut self.pic.y, stride, x0 + bx * 4, y0 + by * 4, 4, mode, av);
                    if self.pic.info[self.addr].nz[blk] > 0 {
                        let mut c = self.coef[blk];
                        transform::dequant4(&mut c, qp, &self.ls4[0][(qp % 6) as usize], false);
                        let off = (y0 + by * 4) * stride + x0 + bx * 4;
                        transform::idct4_add(&mut c, &mut self.pic.y, off, stride);
                    }
                }
            }
            KIND_I8 => {
                for b8 in 0..4 {
                    let (bx, by) = ((b8 % 2) * 2, (b8 / 2) * 2);
                    let av = self.luma_avail(bx, by, 2);
                    let mode = self.pic.info[self.addr].intra_modes[by * 4 + bx];
                    intra::pred_nxn(&mut self.pic.y, stride, x0 + bx * 4, y0 + by * 4, 8, mode, av);
                    if cbp & (1 << b8) != 0 {
                        let mut c = self.coef8[b8];
                        transform::dequant8(&mut c, qp, &self.ls8[0][(qp % 6) as usize]);
                        let off = (y0 + by * 4) * stride + x0 + bx * 4;
                        transform::idct8_add(&mut c, &mut self.pic.y, off, stride);
                    }
                }
            }
            _ => {
                let a = self.intra_avail(self.mb_at(-1, 0));
                let b = self.intra_avail(self.mb_at(0, -1));
                let d = self.intra_avail(self.mb_at(-1, -1));
                let av = Avail { left: a, top: b, topright: false, topleft: d };
                intra::pred16(&mut self.pic.y, stride, x0, y0, i16_mode, av);
                self.luma_residual(true, KIND_I16);
            }
        }
        let a = self.intra_avail(self.mb_at(-1, 0));
        let b = self.intra_avail(self.mb_at(0, -1));
        let d = self.intra_avail(self.mb_at(-1, -1));
        let av = Avail { left: a, top: b, topright: false, topleft: d };
        let cs = self.pic.mb_w * 8;
        intra::pred_chroma(&mut self.pic.cb, cs, self.mb_x * 8, self.mb_y * 8, cm, av);
        intra::pred_chroma(&mut self.pic.cr, cs, self.mb_x * 8, self.mb_y * 8, cm, av);
        self.chroma_residual(true, cbp);
    }

    /// Residual for I16x16 and inter macroblocks.
    fn luma_residual(&mut self, intra: bool, kind: u8) {
        let stride = self.pic.mb_w * 16;
        let x0 = self.mb_x * 16;
        let y0 = self.mb_y * 16;
        let qp = self.qp;
        let info = self.pic.info[self.addr];
        let list = if intra { 0 } else { 3 };
        let ls = self.ls4[list][(qp % 6) as usize];
        if kind == KIND_I16 {
            transform::luma_dc_dequant(&mut self.dc, qp, ls[0]);
            for blk in 0..16 {
                let off = (y0 + (blk / 4) * 4) * stride + x0 + (blk % 4) * 4;
                if info.nz[blk] > 0 {
                    let mut c = self.coef[blk];
                    transform::dequant4(&mut c, qp, &ls, true);
                    c[0] = self.dc[blk];
                    transform::idct4_add(&mut c, &mut self.pic.y, off, stride);
                } else if self.dc[blk] != 0 {
                    transform::idct4_dc_add(self.dc[blk], &mut self.pic.y, off, stride, 4);
                }
            }
            return;
        }
        if info.cbp & 15 == 0 {
            return;
        }
        if info.t8x8 {
            let ls8 = &self.ls8[if intra { 0 } else { 1 }][(qp % 6) as usize];
            for b8 in 0..4 {
                if info.cbp & (1 << b8) == 0 {
                    continue;
                }
                let mut c = self.coef8[b8];
                transform::dequant8(&mut c, qp, ls8);
                let off = (y0 + (b8 / 2) * 8) * stride + x0 + (b8 % 2) * 8;
                transform::idct8_add(&mut c, &mut self.pic.y, off, stride);
            }
        } else {
            for blk in 0..16 {
                if info.nz[blk] == 0 {
                    continue;
                }
                let mut c = self.coef[blk];
                transform::dequant4(&mut c, qp, &ls, false);
                let off = (y0 + (blk / 4) * 4) * stride + x0 + (blk % 4) * 4;
                transform::idct4_add(&mut c, &mut self.pic.y, off, stride);
            }
        }
    }

    fn chroma_residual(&mut self, intra: bool, cbp: u8) {
        if cbp >> 4 == 0 {
            return;
        }
        let cs = self.pic.mb_w * 8;
        let info = self.pic.info[self.addr];
        for c in 0..2 {
            let qpc = chroma_qp(self.qp, self.pps.chroma_qp_offset[c]);
            let list = if intra { 1 + c } else { 4 + c };
            let ls = self.ls4[list][(qpc % 6) as usize];
            let mut dc = self.cdc[c];
            transform::chroma_dc_dequant(&mut dc, qpc, ls[0]);
            for b in 0..4 {
                let off = (self.mb_y * 8 + (b / 2) * 4) * cs + self.mb_x * 8 + (b % 2) * 4;
                let plane = if c == 0 { &mut self.pic.cb } else { &mut self.pic.cr };
                if cbp >> 4 == 2 && info.nz[16 + c * 4 + b] > 0 {
                    let mut co = self.cac[c * 4 + b];
                    transform::dequant4(&mut co, qpc, &ls, true);
                    co[0] = dc[b];
                    transform::idct4_add(&mut co, plane, off, cs);
                } else if dc[b] != 0 {
                    transform::idct4_dc_add(dc[b], plane, off, cs, 4);
                }
            }
        }
    }

    // -----------------------------------------------------------------
    // Inter macroblocks
    // -----------------------------------------------------------------

    /// (ref_idx, mv) of the neighbouring 4x4 block, None if unavailable.
    fn nb_motion(&self, list: usize, bx: isize, by: isize) -> Option<(i8, [i16; 2])> {
        let (m, blk) = self.nb4(bx, by)?;
        let i = &self.pic.info[m];
        if i.is_intra() {
            return Some((-1, [0, 0]));
        }
        let r = i.ref_idx[list][b8_of(blk)];
        if r < 0 { Some((-1, [0, 0])) } else { Some((r, i.mv[list][blk])) }
    }

    /// Neighbour C of a partition starting at (bx, by) with width w,
    /// replaced by D when unavailable.
    fn nb_c(&self, list: usize, bx: isize, by: isize, w: isize) -> Option<(i8, [i16; 2])> {
        let cx = bx + w;
        let cy = by - 1;
        let avail = if (0..4).contains(&cx) && (0..4).contains(&cy) {
            BLK[(cy * 4 + cx) as usize] < BLK[(by * 4 + bx) as usize]
        } else {
            true
        };
        let c = if avail { self.nb_motion(list, cx, cy) } else { None };
        c.or_else(|| self.nb_motion(list, bx - 1, by - 1))
    }

    /// Motion vector predictor (8.4.1.3). `shape`: 0 = median, 1/2 = 16x8
    /// top/bottom, 3/4 = 8x16 left/right.
    fn mvp(&self, list: usize, r: i8, bx: isize, by: isize, w: isize, shape: u8) -> [i16; 2] {
        let a = self.nb_motion(list, bx - 1, by);
        let mut b = self.nb_motion(list, bx, by - 1);
        let mut c = self.nb_c(list, bx, by, w);
        if b.is_none() && c.is_none() && a.is_some() {
            b = a;
            c = a;
        }
        let (ra, ma) = a.unwrap_or((-1, [0, 0]));
        let (rb, mb) = b.unwrap_or((-1, [0, 0]));
        let (rc, mc) = c.unwrap_or((-1, [0, 0]));
        match shape {
            1 if rb == r => return mb,
            2 if ra == r => return ma,
            3 if ra == r => return ma,
            4 if rc == r => return mc,
            _ => {}
        }
        let matches = (ra == r) as u8 + (rb == r) as u8 + (rc == r) as u8;
        if matches == 1 {
            if ra == r {
                return ma;
            }
            if rb == r {
                return mb;
            }
            return mc;
        }
        let med = |x: i16, y: i16, z: i16| x.max(y).min(x.min(y).max(z));
        [med(ma[0], mb[0], mc[0]), med(ma[1], mb[1], mc[1])]
    }

    fn read_ref(&mut self, ent: &mut Ent, list: usize, bx: isize, by: isize) -> Result<i8> {
        let n = self.hdr.num_ref_idx[list];
        if n <= 1 {
            return Ok(0);
        }
        let v = match ent {
            Ent::Cabac(c) => {
                let cond = |s: &Self, x: isize, y: isize| -> usize {
                    match s.nb4(x, y) {
                        Some((m, blk)) => {
                            let i = &s.pic.info[m];
                            let b8 = b8_of(blk);
                            (!i.is_intra() && !i.is_skip() && i.direct8 & (1 << b8) == 0 && i.ref_idx[list][b8] > 0) as usize
                        }
                        None => 0,
                    }
                };
                let inc = cond(self, bx - 1, by) + 2 * cond(self, bx, by - 1);
                c.ref_idx(inc)
            }
            Ent::Cavlc(br) => {
                if n == 2 {
                    1 - br.bit()
                } else {
                    br.ue()
                }
            }
        };
        if v >= n {
            return Err(Error::Invalid("ref_idx"));
        }
        Ok(v as i8)
    }

    fn read_mvd(&mut self, ent: &mut Ent, list: usize, p: Part) -> Result<()> {
        let (bx, by) = (p.x as isize, p.y as isize);
        let mut v = [0i32; 2];
        for comp in 0..2 {
            v[comp] = match ent {
                Ent::Cabac(c) => {
                    let get = |s: &Self, x: isize, y: isize| -> u32 {
                        s.nb4(x, y).map(|(m, blk)| s.pic.info[m].mvd[list][blk][comp] as u32).unwrap_or(0)
                    };
                    let sum = get(self, bx - 1, by) + get(self, bx, by - 1);
                    c.mvd(comp, sum)
                }
                Ent::Cavlc(br) => br.se(),
            };
        }
        let abs = [v[0].unsigned_abs().min(64) as u8, v[1].unsigned_abs().min(64) as u8];
        for y in p.y..p.y + p.h {
            for x in p.x..p.x + p.w {
                self.mvd[list][y * 4 + x] = v;
                self.pic.info[self.addr].mvd[list][y * 4 + x] = abs;
            }
        }
        Ok(())
    }

    fn set_motion(&mut self, p: Part, list: usize, r: i8, mv: [i16; 2]) {
        let id = if r >= 0 { self.lists[list].get(r as usize).map(|x| x.id()).unwrap_or(NO_REF) } else { NO_REF };
        let info = &mut self.pic.info[self.addr];
        for y in p.y..p.y + p.h {
            for x in p.x..p.x + p.w {
                info.mv[list][y * 4 + x] = if r >= 0 { mv } else { [0, 0] };
                let b8 = b8_of(y * 4 + x);
                info.ref_idx[list][b8] = r;
                info.ref_id[list][b8] = id;
            }
        }
    }

    fn skip_mb(&mut self) -> Result<()> {
        self.last_dqp = false;
        let q = self.qp as u8;
        if self.hdr.slice_type == SliceType::P {
            self.cur().kind = KIND_PSKIP;
            let a = self.nb_motion(0, -1, 0);
            let b = self.nb_motion(0, 0, -1);
            let zero = a.is_none() || b.is_none() || a == Some((0, [0, 0])) || b == Some((0, [0, 0]));
            let mv = if zero { [0, 0] } else { self.mvp(0, 0, 0, 0, 4, 0) };
            let p = Part { x: 0, y: 0, w: 4, h: 4 };
            self.set_motion(p, 0, 0, mv);
            self.predict(p, [0, -1], [mv, [0, 0]])?;
        } else {
            let i = self.cur();
            i.kind = KIND_BSKIP;
            i.direct16 = true;
            i.direct8 = 15;
            self.direct_16x16()?;
        }
        self.cur().qp = q;
        Ok(())
    }

    fn inter_mb(&mut self, ent: &mut Ent, t: u32) -> Result<()> {
        let b = self.hdr.slice_type == SliceType::B;
        self.cur().kind = KIND_INTER;
        self.mvd = [[[0; 2]; 16]; 2];
        // (partition shape, prediction per partition): 0 16x16, 1 16x8, 2 8x16, 3 8x8.
        let (shape, preds, ref0): (u8, [u8; 2], bool) = if !b {
            match t {
                0 => (0, [1, 0], false),
                1 => (1, [1, 1], false),
                2 => (2, [1, 1], false),
                3 => (3, [0, 0], false),
                _ => (3, [0, 0], true),
            }
        } else {
            const B_TYPES: [(u8, [u8; 2]); 22] = [
                (0, [0, 0]),
                (0, [1, 0]),
                (0, [2, 0]),
                (0, [3, 0]),
                (1, [1, 1]),
                (2, [1, 1]),
                (1, [2, 2]),
                (2, [2, 2]),
                (1, [1, 2]),
                (2, [1, 2]),
                (1, [2, 1]),
                (2, [2, 1]),
                (1, [1, 3]),
                (2, [1, 3]),
                (1, [2, 3]),
                (2, [2, 3]),
                (1, [3, 1]),
                (2, [3, 1]),
                (1, [3, 2]),
                (2, [3, 2]),
                (1, [3, 3]),
                (2, [3, 3]),
            ];
            if t == 22 {
                (3, [0, 0], false)
            } else if t == 0 {
                (4, [0, 0], false)
            } else {
                let (s, p) = B_TYPES[t as usize];
                (s, p, false)
            }
        };
        let mut no_sub_lt8 = true;
        if shape == 4 {
            // B_Direct_16x16
            let i = self.cur();
            i.direct16 = true;
            i.direct8 = 15;
            self.direct_16x16()?;
            if !self.sps.direct_8x8_inference {
                no_sub_lt8 = false;
            }
        } else if shape == 3 {
            // 8x8 with sub-macroblock types: (shape 0 8x8 1 8x4 2 4x8 3 4x4, pred; pred 0 = direct)
            let mut subs = [(0u8, 0u8); 4];
            for s in subs.iter_mut() {
                let st = match ent {
                    Ent::Cabac(c) => {
                        if b {
                            c.sub_mb_type_b()
                        } else {
                            c.sub_mb_type_p()
                        }
                    }
                    Ent::Cavlc(br) => br.ue(),
                };
                *s = if b {
                    const B_SUB: [(u8, u8); 13] =
                        [(0, 0), (0, 1), (0, 2), (0, 3), (1, 1), (2, 1), (1, 2), (2, 2), (1, 3), (2, 3), (3, 1), (3, 2), (3, 3)];
                    *B_SUB.get(st as usize).ok_or(Error::Invalid("sub_mb_type"))?
                } else {
                    if st > 3 {
                        return Err(Error::Invalid("sub_mb_type"));
                    }
                    (st as u8, 1)
                };
            }
            for (i, s) in subs.iter().enumerate() {
                if s.1 == 0 {
                    self.cur().direct8 |= 1 << i;
                    if !self.sps.direct_8x8_inference {
                        no_sub_lt8 = false;
                    }
                } else if s.0 != 0 {
                    no_sub_lt8 = false;
                }
            }
            let mut refs = [[-1i8; 4]; 2];
            for list in 0..2 {
                for i in 0..4 {
                    let (_, pred) = subs[i];
                    if pred & (1 << list) != 0 {
                        let (bx, by) = ((i % 2) * 2, (i / 2) * 2);
                        refs[list][i] = if ref0 { 0 } else { self.read_ref(ent, list, bx as isize, by as isize)? };
                        let r = refs[list][i];
                        let info = &mut self.pic.info[self.addr];
                        info.ref_idx[list][i] = r;
                    }
                }
            }
            for list in 0..2 {
                for i in 0..4 {
                    let (sh, pred) = subs[i];
                    if pred & (1 << list) == 0 {
                        continue;
                    }
                    for p in sub_parts(i, sh) {
                        self.read_mvd(ent, list, p)?;
                    }
                }
            }
            // Motion vectors, in decoding order.
            for i in 0..4 {
                let (sh, pred) = subs[i];
                if pred == 0 {
                    self.direct_8x8(i)?;
                    continue;
                }
                for p in sub_parts(i, sh) {
                    let mut mvs = [[0i16; 2]; 2];
                    let mut rr = [-1i8; 2];
                    for list in 0..2 {
                        if pred & (1 << list) == 0 {
                            continue;
                        }
                        let r = refs[list][i];
                        let mvp = self.mvp(list, r, p.x as isize, p.y as isize, p.w as isize, 0);
                        let d = self.mvd[list][p.y * 4 + p.x];
                        let mv = [(mvp[0] as i32 + d[0]) as i16, (mvp[1] as i32 + d[1]) as i16];
                        self.set_motion(p, list, r, mv);
                        mvs[list] = mv;
                        rr[list] = r;
                    }
                    for list in 0..2 {
                        if pred & (1 << list) == 0 {
                            self.set_motion(p, list, -1, [0, 0]);
                        }
                    }
                    self.predict(p, rr, mvs)?;
                }
            }
        } else {
            let parts: &[Part] = match shape {
                0 => &[Part { x: 0, y: 0, w: 4, h: 4 }],
                1 => &[Part { x: 0, y: 0, w: 4, h: 2 }, Part { x: 0, y: 2, w: 4, h: 2 }],
                _ => &[Part { x: 0, y: 0, w: 2, h: 4 }, Part { x: 2, y: 0, w: 2, h: 4 }],
            };
            let mut refs = [[-1i8; 2]; 2];
            for list in 0..2 {
                for (k, p) in parts.iter().enumerate() {
                    if preds[k] & (1 << list) != 0 {
                        let r = self.read_ref(ent, list, p.x as isize, p.y as isize)?;
                        refs[list][k] = r;
                        let info = &mut self.pic.info[self.addr];
                        for y in p.y..p.y + p.h {
                            for x in p.x..p.x + p.w {
                                info.ref_idx[list][b8_of(y * 4 + x)] = r;
                            }
                        }
                    }
                }
            }
            for list in 0..2 {
                for (k, p) in parts.iter().enumerate() {
                    if preds[k] & (1 << list) != 0 {
                        self.read_mvd(ent, list, *p)?;
                    }
                }
            }
            for (k, p) in parts.iter().enumerate() {
                let mut mvs = [[0i16; 2]; 2];
                for list in 0..2 {
                    if preds[k] & (1 << list) == 0 {
                        self.set_motion(*p, list, -1, [0, 0]);
                        continue;
                    }
                    let r = refs[list][k];
                    let sh = match (shape, k) {
                        (1, 0) => 1,
                        (1, _) => 2,
                        (2, 0) => 3,
                        (2, _) => 4,
                        _ => 0,
                    };
                    let mvp = self.mvp(list, r, p.x as isize, p.y as isize, p.w as isize, sh);
                    let d = self.mvd[list][p.y * 4 + p.x];
                    let mv = [(mvp[0] as i32 + d[0]) as i16, (mvp[1] as i32 + d[1]) as i16];
                    self.set_motion(*p, list, r, mv);
                    mvs[list] = mv;
                }
                self.predict(*p, [refs[0][k], refs[1][k]], mvs)?;
            }
        }
        let cbp = self.read_cbp(ent, false)?;
        self.cur().cbp = cbp;
        if cbp & 15 != 0 && self.pps.transform_8x8_mode && no_sub_lt8 && self.read_t8(ent) {
            self.cur().t8x8 = true;
        }
        if cbp != 0 {
            self.read_dqp(ent)?;
            self.residual(ent, KIND_INTER, cbp)?;
        } else {
            self.last_dqp = false;
        }
        let q = self.qp as u8;
        self.cur().qp = q;
        self.luma_residual(false, KIND_INTER);
        self.chroma_residual(false, cbp);
        Ok(())
    }

    // -----------------------------------------------------------------
    // Direct prediction
    // -----------------------------------------------------------------

    /// Co-located block: (intra, mv, ref_idx, ref picture id).
    fn colocated(&self, blk: usize) -> (bool, [i16; 2], i8, u32) {
        let Some(r1) = self.lists[1].first() else { return (true, [0, 0], -1, NO_REF) };
        let Some(col) = r1.buf.info.get(self.addr) else { return (true, [0, 0], -1, NO_REF) };
        if col.is_intra() || col.slice == 0 {
            return (true, [0, 0], -1, NO_REF);
        }
        let b8 = b8_of(blk);
        if col.ref_idx[0][b8] >= 0 {
            (false, col.mv[0][blk], col.ref_idx[0][b8], col.ref_id[0][b8])
        } else {
            (false, col.mv[1][blk], col.ref_idx[1][b8], col.ref_id[1][b8])
        }
    }

    fn direct_8x8(&mut self, b8: usize) -> Result<()> {
        self.direct_motion(b8)?;
        let (bx, by) = ((b8 % 2) * 2, (b8 / 2) * 2);
        self.predict_uniform(Part { x: bx, y: by, w: 2, h: 2 })
    }

    /// Direct prediction for a whole macroblock (B_Skip, B_Direct_16x16).
    fn direct_16x16(&mut self) -> Result<()> {
        for b8 in 0..4 {
            self.direct_motion(b8)?;
        }
        self.predict_uniform(Part { x: 0, y: 0, w: 4, h: 4 })
    }

    /// Motion compensate an area from the motion stored for it, as one
    /// block when all its 4x4 blocks share the same motion.
    fn predict_uniform(&mut self, p: Part) -> Result<()> {
        let first = p.y * 4 + p.x;
        let key = |s: &Self, blk: usize| {
            let info = &s.pic.info[s.addr];
            (info.ref_idx[0][b8_of(blk)], info.ref_idx[1][b8_of(blk)], info.mv[0][blk], info.mv[1][blk])
        };
        let k0 = key(self, first);
        let mut same = true;
        for y in p.y..p.y + p.h {
            for x in p.x..p.x + p.w {
                if key(self, y * 4 + x) != k0 {
                    same = false;
                }
            }
        }
        if same {
            return self.predict(p, [k0.0, k0.1], [k0.2, k0.3]);
        }
        if p.w == 4 {
            for b8 in 0..4 {
                self.predict_uniform(Part { x: (b8 % 2) * 2, y: (b8 / 2) * 2, w: 2, h: 2 })?;
            }
            return Ok(());
        }
        for y in p.y..p.y + p.h {
            for x in p.x..p.x + p.w {
                let k = key(self, y * 4 + x);
                self.predict(Part { x, y, w: 1, h: 1 }, [k.0, k.1], [k.2, k.3])?;
            }
        }
        Ok(())
    }

    fn direct_motion(&mut self, b8: usize) -> Result<()> {
        if self.lists[1].is_empty() || self.lists[0].is_empty() {
            return Err(Error::MissingRef);
        }
        let (bx, by) = ((b8 % 2) * 2, (b8 / 2) * 2);
        let inference = self.sps.direct_8x8_inference;
        let blocks: &[(usize, usize)] = if inference { &[(0, 0)] } else { &[(0, 0), (1, 0), (0, 1), (1, 1)] };
        let size = if inference { 2 } else { 1 };
        if self.hdr.direct_spatial {
            let (refs, mvp) = match self.spatial {
                Some(v) => v,
                None => {
                    let v = self.spatial_params();
                    self.spatial = Some(v);
                    v
                }
            };
            let l1_short = !self.lists[1][0].long_term;
            for &(dx, dy) in blocks {
                let p = Part { x: bx + dx, y: by + dy, w: size, h: size };
                let cblk = if inference { (by + (b8 / 2)) * 4 + bx + (b8 % 2) } else { p.y * 4 + p.x };
                let cblk = if inference { corner(b8) } else { cblk };
                let (refs, mvs) = if refs[0] < 0 && refs[1] < 0 {
                    ([0i8, 0], [[0i16; 2]; 2])
                } else {
                    let (_, mv_col, ref_col, _) = self.colocated(cblk);
                    let col_zero = l1_short && ref_col == 0 && (-1..=1).contains(&mv_col[0]) && (-1..=1).contains(&mv_col[1]);
                    let mut mvs = [[0i16; 2]; 2];
                    for l in 0..2 {
                        if refs[l] >= 0 && !(refs[l] == 0 && col_zero) {
                            mvs[l] = mvp[l];
                        }
                    }
                    (refs, mvs)
                };
                for l in 0..2 {
                    self.set_motion(p, l, refs[l], mvs[l]);
                }
            }
        } else {
            for &(dx, dy) in blocks {
                let p = Part { x: bx + dx, y: by + dy, w: size, h: size };
                let cblk = if inference { corner(b8) } else { p.y * 4 + p.x };
                let (intra, mv_col, ref_col, ref_id) = self.colocated(cblk);
                let mv_col = if intra { [0, 0] } else { mv_col };
                let r0 = if ref_col < 0 {
                    0
                } else {
                    self.lists[0].iter().position(|r| r.id() == ref_id).unwrap_or(0) as i8
                };
                let pic0 = &self.lists[0][r0 as usize];
                let pic1 = &self.lists[1][0];
                let tb = (self.pic.poc - pic0.poc()).clamp(-128, 127);
                let td = (pic1.poc() - pic0.poc()).clamp(-128, 127);
                let (m0, m1) = if pic0.long_term || td == 0 {
                    (mv_col, [0i16, 0])
                } else {
                    let tx = (16384 + (td / 2).abs()) / td;
                    let dsf = ((tb * tx + 32) >> 6).clamp(-1024, 1023);
                    let m0 = [((dsf * mv_col[0] as i32 + 128) >> 8) as i16, ((dsf * mv_col[1] as i32 + 128) >> 8) as i16];
                    (m0, [m0[0] - mv_col[0], m0[1] - mv_col[1]])
                };
                self.set_motion(p, 0, r0, m0);
                self.set_motion(p, 1, 0, m1);
            }
        }
        Ok(())
    }

    /// Reference indices and motion vector predictors for spatial direct.
    fn spatial_params(&self) -> ([i8; 2], [[i16; 2]; 2]) {
        let mut refs = [-1i8; 2];
        let mut mvs = [[0i16; 2]; 2];
        let minpos = |a: i8, b: i8| if a >= 0 && b >= 0 { a.min(b) } else { a.max(b) };
        for l in 0..2 {
            let a = self.nb_motion(l, -1, 0).map(|x| x.0).unwrap_or(-1);
            let b = self.nb_motion(l, 0, -1).map(|x| x.0).unwrap_or(-1);
            let c = self.nb_c(l, 0, 0, 4).map(|x| x.0).unwrap_or(-1);
            refs[l] = minpos(a, minpos(b, c));
        }
        if refs[0] < 0 && refs[1] < 0 {
            return (refs, mvs);
        }
        for l in 0..2 {
            if refs[l] >= 0 {
                mvs[l] = self.mvp(l, refs[l], 0, 0, 4, 0);
            }
        }
        (refs, mvs)
    }

    // -----------------------------------------------------------------
    // Motion compensation
    // -----------------------------------------------------------------

    fn predict(&mut self, p: Part, refs: [i8; 2], mvs: [[i16; 2]; 2]) -> Result<()> {
        let w = p.w * 4;
        let h = p.h * 4;
        let px = self.mb_x * 16 + p.x * 4;
        let py = self.mb_y * 16 + p.y * 4;
        let used = [refs[0] >= 0, refs[1] >= 0];
        if !used[0] && !used[1] {
            return Err(Error::MissingRef);
        }
        let lists = self.lists;
        let mut src: [Option<&PicBuf>; 2] = [None, None];
        for l in 0..2 {
            if used[l] {
                src[l] = Some(&lists[l].get(refs[l] as usize).ok_or(Error::MissingRef)?.buf);
            }
        }
        let st = self.hdr.slice_type;
        let explicit = self.hdr.weights.is_some() && ((st == SliceType::P && self.pps.weighted_pred) || (st == SliceType::B && self.pps.weighted_bipred_idc == 1));
        let implicit = st == SliceType::B && self.pps.weighted_bipred_idc == 2 && used[0] && used[1];
        let stride = self.pic.mb_w * 16;
        let cs = self.pic.mb_w * 8;
        let mc = |buf: &PicBuf, mv: [i16; 2], ly: &mut [u8], lo: usize, ls: usize, cb: &mut [u8], cr: &mut [u8], co: usize, css: usize| {
            let lx = px as i32 + (mv[0] as i32 >> 2);
            let lyy = py as i32 + (mv[1] as i32 >> 2);
            inter::luma(&buf.y, buf.width, buf.width, buf.height, lx, lyy, (mv[0] & 3) as u32, (mv[1] & 3) as u32, w, h, ly, lo, ls);
            let cw = buf.width / 2;
            let ch = buf.height / 2;
            let cx = (px / 2) as i32 + (mv[0] as i32 >> 3);
            let cy = (py / 2) as i32 + (mv[1] as i32 >> 3);
            let (fx, fy) = ((mv[0] & 7) as u32, (mv[1] & 7) as u32);
            inter::chroma(&buf.cb, cw, cw, ch, cx, cy, fx, fy, w / 2, h / 2, cb, co, css);
            inter::chroma(&buf.cr, cw, cw, ch, cx, cy, fx, fy, w / 2, h / 2, cr, co, css);
        };
        let loff = py * stride + px;
        let coff = (py / 2) * cs + px / 2;
        if !explicit && !(used[0] && used[1]) {
            // Single list without weights: predict straight into the picture.
            let l = if used[0] { 0 } else { 1 };
            let pic = &mut *self.pic;
            mc(src[l].unwrap(), mvs[l], &mut pic.y, loff, stride, &mut pic.cb, &mut pic.cr, coff, cs);
            return Ok(());
        }
        let mut luma = [[0u8; 256]; 2];
        let mut cb = [[0u8; 64]; 2];
        let mut cr = [[0u8; 64]; 2];
        for l in 0..2 {
            if let Some(buf) = src[l] {
                let (a, b, c) = (&mut luma[l], &mut cb[l], &mut cr[l]);
                mc(buf, mvs[l], a, 0, 16, b, c, 0, 8);
            }
        }
        // Weights per plane: (w0, w1, o0, o1, logWD).
        let mut wts = [(1i32, 1i32, 0i32, 0i32, 0u32); 3];
        let mode = if explicit {
            let wt = self.hdr.weights.as_ref().unwrap();
            let e0 = if used[0] { wt.table[0].get(refs[0] as usize).copied() } else { None };
            let e1 = if used[1] { wt.table[1].get(refs[1] as usize).copied() } else { None };
            let e0 = e0.unwrap_or([1 << wt.luma_log2, 0, 1 << wt.chroma_log2, 0, 1 << wt.chroma_log2, 0]);
            let e1 = e1.unwrap_or([1 << wt.luma_log2, 0, 1 << wt.chroma_log2, 0, 1 << wt.chroma_log2, 0]);
            for pl in 0..3 {
                let log = if pl == 0 { wt.luma_log2 } else { wt.chroma_log2 };
                wts[pl] = (e0[pl * 2], e1[pl * 2], e0[pl * 2 + 1], e1[pl * 2 + 1], log);
            }
            1
        } else if implicit {
            let n1 = self.lists[1].len();
            let (w0, w1) = self.implicit[refs[0] as usize * n1 + refs[1] as usize];
            for w in wts.iter_mut() {
                *w = (w0, w1, 0, 0, 5);
            }
            1
        } else {
            0
        };
        let planes: [(&mut [u8], usize, usize, &[u8], &[u8], usize, usize, usize); 3] = [
            (&mut self.pic.y[..], loff, stride, &luma[0][..], &luma[1][..], 16, w, h),
            (&mut self.pic.cb[..], coff, cs, &cb[0][..], &cb[1][..], 8, w / 2, h / 2),
            (&mut self.pic.cr[..], coff, cs, &cr[0][..], &cr[1][..], 8, w / 2, h / 2),
        ];
        for (pl, (dst, off, ds, a, b, ss, w, h)) in planes.into_iter().enumerate() {
            let wt = wts[pl];
            for j in 0..h {
                let d = &mut dst[off + j * ds..off + j * ds + w];
                let ra = &a[j * ss..j * ss + w];
                let rb = &b[j * ss..j * ss + w];
                match (mode, used[0], used[1]) {
                    (0, _, _) => {
                        for i in 0..w {
                            d[i] = ((ra[i] as u32 + rb[i] as u32 + 1) >> 1) as u8;
                        }
                    }
                    (_, true, true) => {
                        let (w0, w1, o0, o1, log) = wt;
                        let o = (o0 + o1 + 1) >> 1;
                        for i in 0..w {
                            d[i] = clip_u8(((ra[i] as i32 * w0 + rb[i] as i32 * w1 + (1 << log)) >> (log + 1)) + o);
                        }
                    }
                    (_, used0, _) => {
                        let (wgt, off) = if used0 { (wt.0, wt.2) } else { (wt.1, wt.3) };
                        let r = if used0 { ra } else { rb };
                        let log = wt.4;
                        if log >= 1 {
                            let rnd = 1 << (log - 1);
                            for i in 0..w {
                                d[i] = clip_u8(((r[i] as i32 * wgt + rnd) >> log) + off);
                            }
                        } else {
                            for i in 0..w {
                                d[i] = clip_u8(r[i] as i32 * wgt + off);
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

fn corner(b8: usize) -> usize {
    [0, 3, 12, 15][b8]
}

fn sub_parts(i: usize, sh: u8) -> Vec<Part> {
    let (bx, by) = ((i % 2) * 2, (i / 2) * 2);
    match sh {
        0 => alloc::vec![Part { x: bx, y: by, w: 2, h: 2 }],
        1 => alloc::vec![Part { x: bx, y: by, w: 2, h: 1 }, Part { x: bx, y: by + 1, w: 2, h: 1 }],
        2 => alloc::vec![Part { x: bx, y: by, w: 1, h: 2 }, Part { x: bx + 1, y: by, w: 1, h: 2 }],
        _ => alloc::vec![
            Part { x: bx, y: by, w: 1, h: 1 },
            Part { x: bx + 1, y: by, w: 1, h: 1 },
            Part { x: bx, y: by + 1, w: 1, h: 1 },
            Part { x: bx + 1, y: by + 1, w: 1, h: 1 }
        ],
    }
}
