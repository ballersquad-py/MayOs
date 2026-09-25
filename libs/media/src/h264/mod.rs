//! H.264 / AVC video decoder (ITU-T H.264): Baseline, Main and High
//! profiles, progressive 8-bit 4:2:0, CAVLC and CABAC.

pub mod cabac;
pub mod cavlc;
mod deblock;
mod decode;
mod inter;
mod intra;
pub mod ps;
pub mod slice;
mod transform;
pub mod types;

use alloc::collections::VecDeque;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

use crate::bits::unescape;
use cavlc::CavlcTables;
use ps::{Pps, Sps};
use slice::{Mmco, RefMod, SliceHeader, SliceType};
use types::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    Invalid(&'static str),
    Unsupported(&'static str),
    MissingRef,
}

impl Error {
    pub fn as_str(&self) -> &'static str {
        match self {
            Error::Invalid(s) | Error::Unsupported(s) => s,
            Error::MissingRef => "missing reference picture",
        }
    }
}

pub type Result<T> = core::result::Result<T, Error>;

pub const ZIGZAG4: [u8; 16] = [0, 1, 4, 8, 5, 2, 3, 6, 9, 12, 13, 10, 7, 11, 14, 15];
pub const ZIGZAG8: [u8; 64] = [
    0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5, 12, 19, 26, 33, 40, 48, 41, 34, 27, 20, 13, 6, 7, 14, 21, 28, 35, 42,
    49, 56, 57, 50, 43, 36, 29, 22, 15, 23, 30, 37, 44, 51, 58, 59, 52, 45, 38, 31, 39, 46, 53, 60, 61, 54, 47, 55, 62, 63,
];
/// Decoding order index of 4x4 blocks -> raster index (and back).
pub const BLK: [u8; 16] = [0, 1, 4, 5, 2, 3, 6, 7, 8, 9, 12, 13, 10, 11, 14, 15];

/// A decoded frame ready for display.
#[derive(Clone)]
pub struct Frame {
    pub buf: Arc<PicBuf>,
    pub crop_x: usize,
    pub crop_y: usize,
    pub width: usize,
    pub height: usize,
    pub poc: i32,
    pub full_range: bool,
    /// Matrix coefficients: 1 = BT.709, otherwise BT.601.
    pub matrix: u8,
}

impl Frame {
    pub fn y_plane(&self) -> (&[u8], usize) {
        (&self.buf.y, self.buf.width)
    }

    /// Convert to 0xAARRGGBB pixels (width x height).
    pub fn to_argb(&self, out: &mut [u32]) {
        crate::yuv::to_argb(
            &self.buf.y,
            &self.buf.cb,
            &self.buf.cr,
            self.buf.width,
            self.buf.width / 2,
            self.crop_x,
            self.crop_y,
            self.width,
            self.height,
            self.matrix == 1,
            self.full_range,
            out,
        );
    }
}

#[derive(Clone)]
struct DpbPic {
    buf: Arc<PicBuf>,
    frame_num: u32,
    long_idx: Option<u32>,
    short: bool,
    output: bool,
}

impl DpbPic {
    fn is_ref(&self) -> bool {
        self.short || self.long_idx.is_some()
    }
}

struct Current {
    pic: CurPic,
    sps_id: u32,
    frame_num: u32,
    idr: bool,
    nal_ref_idc: u32,
    first: SliceHeader,
    poc_msb: i32,
    poc_lsb: u32,
    top_poc: i32,
    bottom_poc: i32,
    frame_num_offset: i32,
    has_mmco5: bool,
}

pub struct Decoder {
    sps: Vec<Option<Sps>>,
    pps: Vec<Option<Pps>>,
    length_size: usize,
    cavlc: CavlcTables,
    cur: Option<Current>,
    dpb: Vec<DpbPic>,
    /// Pictures no longer in the DPB; their buffers are reused once nobody
    /// else holds them.
    pool: Vec<Arc<PicBuf>>,
    out: VecDeque<Frame>,
    prev_ref_poc_msb: i32,
    prev_ref_poc_lsb: i32,
    prev_frame_num_offset: i32,
    prev_frame_num: u32,
    prev_had_mmco5: bool,
    max_long_idx: Option<u32>,
    next_id: u32,
    /// Waiting for a picture that can be decoded without missing references.
    need_key: bool,
    active_sps: Option<u32>,
    /// Skip pictures with nal_ref_idc == 0 (used when playback runs late).
    pub skip_nonref: bool,
    /// Access units dropped by `skip_nonref` (so callers can drop their
    /// timestamps too).
    pub skipped_pictures: u64,
    skipped_in_au: bool,
    decoded_in_au: bool,
    /// Number of slices that failed to decode (for diagnostics).
    pub errors: u32,
    pub last_error: Option<Error>,
}

impl Default for Decoder {
    fn default() -> Self {
        Self::new()
    }
}

impl Decoder {
    pub fn new() -> Decoder {
        Decoder {
            sps: vec![None; 32],
            pps: vec![None; 256],
            length_size: 0,
            cavlc: CavlcTables::new(),
            cur: None,
            dpb: Vec::new(),
            pool: Vec::new(),
            out: VecDeque::new(),
            prev_ref_poc_msb: 0,
            prev_ref_poc_lsb: 0,
            prev_frame_num_offset: 0,
            prev_frame_num: 0,
            prev_had_mmco5: false,
            max_long_idx: None,
            next_id: 1,
            need_key: true,
            skip_nonref: false,
            skipped_pictures: 0,
            skipped_in_au: false,
            decoded_in_au: false,
            active_sps: None,
            errors: 0,
            last_error: None,
        }
    }

    /// Configure from an MP4/MKV `avcC` record: stores the parameter sets
    /// and switches to length-prefixed NAL units.
    pub fn configure_avcc(&mut self, avcc: &[u8]) -> Result<()> {
        if avcc.len() < 7 || avcc[0] != 1 {
            return Err(Error::Invalid("avcC"));
        }
        self.length_size = (avcc[4] & 3) as usize + 1;
        let mut p = 5;
        let nsps = (avcc[p] & 0x1f) as usize;
        p += 1;
        for _ in 0..nsps {
            let len = u16::from_be_bytes([*avcc.get(p).ok_or(Error::Invalid("avcC"))?, *avcc.get(p + 1).ok_or(Error::Invalid("avcC"))?]) as usize;
            p += 2;
            let nal = avcc.get(p..p + len).ok_or(Error::Invalid("avcC"))?;
            self.nal(nal)?;
            p += len;
        }
        let npps = *avcc.get(p).ok_or(Error::Invalid("avcC"))? as usize;
        p += 1;
        for _ in 0..npps {
            let len = u16::from_be_bytes([*avcc.get(p).ok_or(Error::Invalid("avcC"))?, *avcc.get(p + 1).ok_or(Error::Invalid("avcC"))?]) as usize;
            p += 2;
            let nal = avcc.get(p..p + len).ok_or(Error::Invalid("avcC"))?;
            self.nal(nal)?;
            p += len;
        }
        Ok(())
    }

    /// Picture size (width, height) after cropping, once an SPS is known.
    pub fn size(&self) -> Option<(usize, usize)> {
        let id = self.active_sps.or_else(|| self.sps.iter().position(|s| s.is_some()).map(|i| i as u32))?;
        let s = self.sps[id as usize].as_ref()?;
        let w = s.width_mbs as usize * 16 - (s.crop[0] + s.crop[1]) as usize;
        let h = s.height_mbs as usize * 16 - (s.crop[2] + s.crop[3]) as usize;
        Some((w, h))
    }

    /// Decode one access unit (MP4 sample) or a chunk of an Annex B stream.
    pub fn decode(&mut self, data: &[u8]) -> Result<()> {
        self.skipped_in_au = false;
        self.decoded_in_au = false;
        let r = self.decode_au(data);
        if self.skipped_in_au && !self.decoded_in_au {
            self.skipped_pictures += 1;
        }
        r
    }

    fn decode_au(&mut self, data: &[u8]) -> Result<()> {
        if self.length_size > 0 && !starts_with_start_code(data) {
            let mut p = 0;
            while p + self.length_size <= data.len() {
                let mut len = 0usize;
                for i in 0..self.length_size {
                    len = (len << 8) | data[p + i] as usize;
                }
                p += self.length_size;
                if p + len > data.len() {
                    return Err(Error::Invalid("NAL length"));
                }
                let r = self.nal(&data[p..p + len]);
                if let Err(e) = r {
                    self.errors += 1;
                    self.last_error = Some(e);
                }
                p += len;
            }
            // An MP4 sample is a complete access unit.
            self.finish_picture();
        } else {
            for nal in AnnexB::new(data) {
                let r = self.nal(nal);
                if let Err(e) = r {
                    self.errors += 1;
                    self.last_error = Some(e);
                }
            }
        }
        Ok(())
    }

    /// Decode a single NAL unit (without start code or length prefix).
    pub fn decode_nal(&mut self, nal: &[u8]) {
        if let Err(e) = self.nal(nal) {
            self.errors += 1;
            self.last_error = Some(e);
        }
    }

    /// End the current picture (for containers with one access unit per
    /// packet but Annex B framing).
    pub fn flush_picture(&mut self) {
        self.finish_picture();
    }

    /// Next frame in display order, if one is ready.
    pub fn next_frame(&mut self) -> Option<Frame> {
        self.out.pop_front()
    }

    /// Finish the current picture and output everything still buffered.
    pub fn flush(&mut self) {
        self.finish_picture();
        while self.bump() {}
    }

    /// Forget all state except parameter sets (e.g. after seeking).
    pub fn reset(&mut self) {
        self.cur = None;
        self.dpb.clear();
        self.out.clear();
        self.need_key = true;
        self.prev_ref_poc_msb = 0;
        self.prev_ref_poc_lsb = 0;
        self.prev_frame_num_offset = 0;
        self.prev_frame_num = 0;
        self.max_long_idx = None;
    }

    fn nal(&mut self, nal: &[u8]) -> Result<()> {
        if nal.is_empty() {
            return Ok(());
        }
        let hdr = nal[0];
        if hdr & 0x80 != 0 {
            return Err(Error::Invalid("forbidden_zero_bit"));
        }
        let nal_ref_idc = ((hdr >> 5) & 3) as u32;
        let t = hdr & 0x1f;
        match t {
            1 | 5 => {
                if self.skip_nonref && nal_ref_idc == 0 {
                    self.skipped_in_au = true;
                    return Ok(());
                }
                self.decoded_in_au = true;
                let rbsp = unescape(&nal[1..]);
                self.slice(&rbsp, t, nal_ref_idc)
            }
            7 => {
                self.finish_picture();
                let rbsp = unescape(&nal[1..]);
                let s = ps::parse_sps(&rbsp)?;
                let id = s.id as usize;
                self.sps[id] = Some(s);
                Ok(())
            }
            8 => {
                self.finish_picture();
                let rbsp = unescape(&nal[1..]);
                let p = ps::parse_pps(&rbsp, &self.sps)?;
                let id = p.id as usize;
                self.pps[id] = Some(p);
                Ok(())
            }
            9 | 10 | 11 => {
                self.finish_picture();
                Ok(())
            }
            2..=4 => Err(Error::Unsupported("data partitioning")),
            _ => Ok(()),
        }
    }

    fn slice(&mut self, rbsp: &[u8], nal_type: u8, nal_ref_idc: u32) -> Result<()> {
        let (first_mb, pps_id) = slice::peek_pps_id(rbsp).ok_or(Error::Invalid("slice"))?;
        let pps = self.pps.get(pps_id as usize).and_then(|p| p.clone()).ok_or(Error::Invalid("slice refers to missing pps"))?;
        let sps = self.sps[pps.sps_id as usize].clone().ok_or(Error::Invalid("pps refers to missing sps"))?;
        let hdr = slice::parse(rbsp, nal_type, nal_ref_idc, &sps, &pps)?;
        // A new picture starts with a slice at macroblock 0 (or with a
        // different frame_num / IDR flag).
        if let Some(c) = &self.cur {
            if first_mb == 0 || c.frame_num != hdr.frame_num || c.idr != hdr.idr || c.sps_id != sps.id {
                self.finish_picture();
            }
        }
        if self.cur.is_none() {
            if self.need_key && hdr.slice_type != SliceType::I {
                return Ok(());
            }
            self.start_picture(&hdr, &sps, &pps, nal_ref_idc);
        }
        let lists = self.build_lists(&hdr, &sps)?;
        let cur = self.cur.as_mut().unwrap();
        let slice_num = cur.pic.slices.len() as u32 + 1;
        cur.pic.slices.push(SliceParams {
            disable: hdr.disable_deblocking,
            alpha: hdr.alpha_offset,
            beta: hdr.beta_offset,
            cqo: pps.chroma_qp_offset,
        });
        let mut dec = decode::SliceDec::new(&sps, &pps, &hdr, &mut cur.pic, &lists, &self.cavlc, slice_num);
        let r = dec.run(rbsp);
        if r.is_ok() && hdr.slice_type == SliceType::I {
            self.need_key = false;
        }
        if let Some(c) = &self.cur {
            if c.pic.decoded >= c.pic.mb_w * c.pic.mb_h && self.length_size == 0 {
                self.finish_picture();
            }
        }
        r
    }

    fn start_picture(&mut self, hdr: &SliceHeader, sps: &Sps, _pps: &Pps, nal_ref_idc: u32) {
        let max_frame_num = 1u32 << sps.log2_max_frame_num;
        if hdr.idr {
            // IDR: previous pictures are output (unless told otherwise) and
            // no longer used for reference.
            if hdr.no_output_of_prior_pics {
                self.dpb.retain(|_| false);
            } else {
                while self.bump() {}
                self.dpb.clear();
            }
            self.max_long_idx = None;
        }
        let mut poc_msb = 0;
        let mut frame_num_offset = 0i32;
        let (top, bottom);
        match sps.poc_type {
            0 => {
                let (prev_msb, prev_lsb) = if hdr.idr { (0, 0) } else { (self.prev_ref_poc_msb, self.prev_ref_poc_lsb) };
                let max_lsb = 1i32 << sps.log2_max_poc_lsb;
                let lsb = hdr.poc_lsb as i32;
                poc_msb = if lsb < prev_lsb && prev_lsb - lsb >= max_lsb / 2 {
                    prev_msb + max_lsb
                } else if lsb > prev_lsb && lsb - prev_lsb > max_lsb / 2 {
                    prev_msb - max_lsb
                } else {
                    prev_msb
                };
                top = poc_msb + lsb;
                bottom = top + hdr.delta_poc_bottom;
            }
            _ => {
                let prev_offset = if self.prev_had_mmco5 { 0 } else { self.prev_frame_num_offset };
                frame_num_offset = if hdr.idr {
                    0
                } else if self.prev_frame_num > hdr.frame_num {
                    prev_offset + max_frame_num as i32
                } else {
                    prev_offset
                };
                if sps.poc_type == 1 {
                    let n = sps.offset_for_ref_frame.len() as i32;
                    let mut abs = if n != 0 { frame_num_offset + hdr.frame_num as i32 } else { 0 };
                    if nal_ref_idc == 0 && abs > 0 {
                        abs -= 1;
                    }
                    let mut expected = 0;
                    if abs > 0 {
                        let delta_cycle: i32 = sps.offset_for_ref_frame.iter().sum();
                        let cycle = (abs - 1) / n;
                        let in_cycle = (abs - 1) % n;
                        expected = cycle * delta_cycle;
                        for i in 0..=in_cycle as usize {
                            expected += sps.offset_for_ref_frame[i];
                        }
                    }
                    if nal_ref_idc == 0 {
                        expected += sps.offset_for_non_ref_pic;
                    }
                    top = expected + hdr.delta_poc[0];
                    bottom = top + sps.offset_for_top_to_bottom_field + hdr.delta_poc[1];
                } else {
                    let t = if hdr.idr {
                        0
                    } else if nal_ref_idc == 0 {
                        2 * (frame_num_offset + hdr.frame_num as i32) - 1
                    } else {
                        2 * (frame_num_offset + hdr.frame_num as i32)
                    };
                    top = t;
                    bottom = t;
                }
            }
        }
        let poc = top.min(bottom);
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1).max(1);
        self.active_sps = Some(sps.id);
        let n = (sps.width_mbs * sps.height_mbs) as usize;
        let mut old = None;
        if let Some(i) = self.pool.iter().position(|p| Arc::strong_count(p) == 1 && p.info.len() == n) {
            old = Arc::try_unwrap(self.pool.swap_remove(i)).ok();
        }
        self.cur = Some(Current {
            pic: CurPic::new(sps.width_mbs as usize, sps.height_mbs as usize, poc, id, old),
            sps_id: sps.id,
            frame_num: hdr.frame_num,
            idr: hdr.idr,
            nal_ref_idc,
            first: hdr.clone(),
            poc_msb,
            poc_lsb: hdr.poc_lsb,
            top_poc: top,
            bottom_poc: bottom,
            frame_num_offset,
            has_mmco5: hdr.has_mmco5(),
        });
    }

    fn pic_num(&self, p: &DpbPic, cur_frame_num: u32, max_frame_num: u32) -> i32 {
        if p.frame_num > cur_frame_num { p.frame_num as i32 - max_frame_num as i32 } else { p.frame_num as i32 }
    }

    fn build_lists(&self, hdr: &SliceHeader, sps: &Sps) -> Result<[Vec<RefPic>; 2]> {
        let mut lists: [Vec<RefPic>; 2] = [Vec::new(), Vec::new()];
        if hdr.slice_type == SliceType::I {
            return Ok(lists);
        }
        let max_frame_num = 1u32 << sps.log2_max_frame_num;
        let cur_fn = hdr.frame_num;
        let cur_poc = self.cur.as_ref().map(|c| c.pic.poc).unwrap_or(0);
        let rp = |p: &DpbPic| RefPic { buf: p.buf.clone(), long_term: p.long_idx.is_some() && !p.short };
        let mut shorts: Vec<&DpbPic> = self.dpb.iter().filter(|p| p.short).collect();
        let mut longs: Vec<&DpbPic> = self.dpb.iter().filter(|p| !p.short && p.long_idx.is_some()).collect();
        longs.sort_by_key(|p| p.long_idx.unwrap());
        if hdr.slice_type == SliceType::P {
            shorts.sort_by_key(|p| -self.pic_num(p, cur_fn, max_frame_num));
            lists[0] = shorts.iter().map(|p| rp(p)).chain(longs.iter().map(|p| rp(p))).collect();
        } else {
            let mut before: Vec<&DpbPic> = shorts.iter().copied().filter(|p| p.buf.poc < cur_poc).collect();
            let mut after: Vec<&DpbPic> = shorts.iter().copied().filter(|p| p.buf.poc > cur_poc).collect();
            before.sort_by_key(|p| -p.buf.poc);
            after.sort_by_key(|p| p.buf.poc);
            lists[0] = before.iter().chain(after.iter()).chain(longs.iter()).map(|p| rp(p)).collect();
            lists[1] = after.iter().chain(before.iter()).chain(longs.iter()).map(|p| rp(p)).collect();
            if lists[1].len() > 1 && lists[0].len() == lists[1].len() && lists[0].iter().zip(lists[1].iter()).all(|(a, b)| a.id() == b.id()) {
                lists[1].swap(0, 1);
            }
        }
        for l in 0..2 {
            let n = hdr.num_ref_idx[l] as usize;
            lists[l].truncate(n);
            if !hdr.ref_mods[l].is_empty() {
                let mut pred = cur_fn as i32;
                let mut idx = 0usize;
                for m in &hdr.ref_mods[l] {
                    let (pic, is_long, num) = match *m {
                        RefMod::Sub(d) | RefMod::Add(d) => {
                            let mut no_wrap = if matches!(m, RefMod::Sub(_)) { pred - (d as i32 + 1) } else { pred + (d as i32 + 1) };
                            if no_wrap < 0 {
                                no_wrap += max_frame_num as i32;
                            } else if no_wrap >= max_frame_num as i32 {
                                no_wrap -= max_frame_num as i32;
                            }
                            pred = no_wrap;
                            let pic_num = if no_wrap > cur_fn as i32 { no_wrap - max_frame_num as i32 } else { no_wrap };
                            let p = self
                                .dpb
                                .iter()
                                .find(|p| p.short && self.pic_num(p, cur_fn, max_frame_num) == pic_num)
                                .ok_or(Error::MissingRef)?;
                            (rp(p), false, pic_num)
                        }
                        RefMod::LongTerm(n) => {
                            let p = self.dpb.iter().find(|p| !p.short && p.long_idx == Some(n)).ok_or(Error::MissingRef)?;
                            (rp(p), true, n as i32)
                        }
                    };
                    let id = pic.id();
                    lists[l].insert(idx.min(lists[l].len()), pic);
                    idx += 1;
                    let mut k = idx;
                    while k < lists[l].len() {
                        let same = lists[l][k].id() == id;
                        let _ = (is_long, num);
                        if same {
                            lists[l].remove(k);
                            break;
                        }
                        k += 1;
                    }
                }
                lists[l].truncate(n);
            }
        }
        if lists[0].is_empty() || (hdr.slice_type == SliceType::B && lists[1].is_empty()) {
            return Err(Error::MissingRef);
        }
        Ok(lists)
    }

    fn finish_picture(&mut self) {
        let Some(mut cur) = self.cur.take() else { return };
        let sps = match self.sps[cur.sps_id as usize].clone() {
            Some(s) => s,
            None => return,
        };
        deblock::deblock_picture(&mut cur.pic);
        let mut poc = cur.pic.poc;
        let mut top = cur.top_poc;
        if cur.has_mmco5 {
            let temp = cur.top_poc.min(cur.bottom_poc);
            top -= temp;
            poc -= temp;
        }
        let w = cur.pic.mb_w * 16;
        let h = cur.pic.mb_h * 16;
        let buf = Arc::new(PicBuf {
            id: cur.pic.id,
            poc,
            width: w,
            height: h,
            y: core::mem::take(&mut cur.pic.y),
            cb: core::mem::take(&mut cur.pic.cb),
            cr: core::mem::take(&mut cur.pic.cr),
            info: core::mem::take(&mut cur.pic.info),
        });
        let max_frame_num = 1u32 << sps.log2_max_frame_num;
        let is_ref = cur.nal_ref_idc != 0;
        let mut long_idx = None;
        let mut short = is_ref;
        if is_ref {
            if cur.idr {
                if cur.first.long_term_reference {
                    long_idx = Some(0);
                    short = false;
                    self.max_long_idx = Some(0);
                } else {
                    self.max_long_idx = None;
                }
            } else if let Some(mmcos) = &cur.first.mmcos {
                let cur_fn = cur.frame_num;
                for m in mmcos {
                    match *m {
                        Mmco::ShortUnused(d) => {
                            let pn = cur_fn as i32 - (d as i32 + 1);
                            for p in self.dpb.iter_mut() {
                                let num = if p.frame_num > cur_fn { p.frame_num as i32 - max_frame_num as i32 } else { p.frame_num as i32 };
                                if p.short && num == pn {
                                    p.short = false;
                                }
                            }
                        }
                        Mmco::LongUnused(n) => {
                            for p in self.dpb.iter_mut() {
                                if !p.short && p.long_idx == Some(n) {
                                    p.long_idx = None;
                                }
                            }
                        }
                        Mmco::ShortToLong(d, idx) => {
                            let pn = cur_fn as i32 - (d as i32 + 1);
                            for p in self.dpb.iter_mut() {
                                if !p.short && p.long_idx == Some(idx) {
                                    p.long_idx = None;
                                }
                            }
                            for p in self.dpb.iter_mut() {
                                let num = if p.frame_num > cur_fn { p.frame_num as i32 - max_frame_num as i32 } else { p.frame_num as i32 };
                                if p.short && num == pn {
                                    p.short = false;
                                    p.long_idx = Some(idx);
                                }
                            }
                        }
                        Mmco::MaxLongIdx(v) => {
                            self.max_long_idx = if v == 0 { None } else { Some(v - 1) };
                            for p in self.dpb.iter_mut() {
                                if let Some(i) = p.long_idx {
                                    if !p.short && self.max_long_idx.map(|m| i > m).unwrap_or(true) {
                                        p.long_idx = None;
                                    }
                                }
                            }
                        }
                        Mmco::ClearAll => {
                            for p in self.dpb.iter_mut() {
                                p.short = false;
                                p.long_idx = None;
                            }
                            self.max_long_idx = None;
                        }
                        Mmco::CurrentToLong(idx) => {
                            for p in self.dpb.iter_mut() {
                                if !p.short && p.long_idx == Some(idx) {
                                    p.long_idx = None;
                                }
                            }
                            long_idx = Some(idx);
                            short = false;
                        }
                    }
                }
                for p in self.dpb.iter_mut() {
                    if p.short {
                        p.long_idx = None;
                    }
                }
            } else {
                // Sliding window.
                let max = sps.max_num_ref_frames.max(1) as usize;
                loop {
                    let nref = self.dpb.iter().filter(|p| p.is_ref()).count();
                    if nref < max {
                        break;
                    }
                    let cur_fn = cur.frame_num;
                    let oldest = self
                        .dpb
                        .iter_mut()
                        .filter(|p| p.short)
                        .min_by_key(|p| if p.frame_num > cur_fn { p.frame_num as i32 - max_frame_num as i32 } else { p.frame_num as i32 });
                    match oldest {
                        Some(p) => p.short = false,
                        None => break,
                    }
                }
            }
        }
        // POC bookkeeping for the next picture.
        if is_ref {
            if cur.has_mmco5 {
                self.prev_ref_poc_msb = 0;
                self.prev_ref_poc_lsb = top;
            } else {
                self.prev_ref_poc_msb = cur.poc_msb;
                self.prev_ref_poc_lsb = cur.poc_lsb as i32;
            }
        }
        self.prev_frame_num_offset = cur.frame_num_offset;
        self.prev_frame_num = if cur.has_mmco5 { 0 } else { cur.frame_num };
        self.prev_had_mmco5 = cur.has_mmco5;
        if cur.has_mmco5 {
            // Behaves like an IDR for output: everything before comes first.
            while self.bump() {}
        }
        // Remove pictures neither referenced nor waiting for output.
        self.prune();
        let dpb_size = sps.max_dec_frame_buffering.unwrap_or(16).max(sps.max_num_ref_frames).clamp(1, 16) as usize;
        while self.dpb.len() >= dpb_size {
            if !self.bump() {
                break;
            }
            self.prune();
        }
        let crop = sps.crop;
        self.dpb.push(DpbPic {
            buf,
            frame_num: if cur.has_mmco5 { 0 } else { cur.frame_num },
            long_idx,
            short: short && long_idx.is_none(),
            output: true,
        });
        let _ = crop;
        let reorder = sps.num_reorder_frames.unwrap_or(dpb_size as u32) as usize;
        while self.dpb.iter().filter(|p| p.output).count() > reorder {
            if !self.bump() {
                break;
            }
        }
        self.prune();
    }

    /// Drop pictures that are neither references nor waiting for output,
    /// keeping their buffers for reuse.
    fn prune(&mut self) {
        let mut i = 0;
        while i < self.dpb.len() {
            if self.dpb[i].is_ref() || self.dpb[i].output {
                i += 1;
            } else {
                let p = self.dpb.remove(i);
                self.pool.push(p.buf);
            }
        }
        // Forget buffers that stayed in use (e.g. held by the caller).
        if self.pool.len() > 6 {
            self.pool.remove(0);
        }
    }

    /// Output the waiting picture with the smallest POC.
    fn bump(&mut self) -> bool {
        let Some(i) = (0..self.dpb.len()).filter(|&i| self.dpb[i].output).min_by_key(|&i| self.dpb[i].buf.poc) else {
            return false;
        };
        self.dpb[i].output = false;
        let buf = self.dpb[i].buf.clone();
        let sps = self.active_sps.and_then(|id| self.sps[id as usize].as_ref());
        let (cx, cy, w, h, full, matrix) = match sps {
            Some(s) => (
                s.crop[0] as usize,
                s.crop[2] as usize,
                buf.width - (s.crop[0] + s.crop[1]) as usize,
                buf.height - (s.crop[2] + s.crop[3]) as usize,
                s.full_range,
                s.matrix,
            ),
            None => (0, 0, buf.width, buf.height, false, 0),
        };
        let poc = buf.poc;
        self.out.push_back(Frame { buf, crop_x: cx, crop_y: cy, width: w, height: h, poc, full_range: full, matrix });
        true
    }
}

fn starts_with_start_code(d: &[u8]) -> bool {
    d.len() >= 4 && (d[..4] == [0, 0, 0, 1] || d[..3] == [0, 0, 1])
}

/// Iterator over NAL units of an Annex B byte stream.
pub struct AnnexB<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> AnnexB<'a> {
    pub fn new(data: &'a [u8]) -> AnnexB<'a> {
        let mut s = AnnexB { data, pos: 0 };
        s.pos = s.find_start(0).map(|p| p + 3).unwrap_or(data.len());
        s
    }

    fn find_start(&self, from: usize) -> Option<usize> {
        let d = self.data;
        let mut i = from;
        while i + 3 <= d.len() {
            if d[i] == 0 && d[i + 1] == 0 && d[i + 2] == 1 {
                return Some(i);
            }
            i += 1;
        }
        None
    }
}

impl<'a> Iterator for AnnexB<'a> {
    type Item = &'a [u8];
    fn next(&mut self) -> Option<&'a [u8]> {
        if self.pos >= self.data.len() {
            return None;
        }
        let start = self.pos;
        let end = match self.find_start(start) {
            Some(e) => {
                self.pos = e + 3;
                e
            }
            None => {
                self.pos = self.data.len();
                self.data.len()
            }
        };
        let mut e = end;
        while e > start && self.data[e - 1] == 0 {
            e -= 1;
        }
        Some(&self.data[start..e])
    }
}
