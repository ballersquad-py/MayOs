//! Slice header (7.3.3).

use alloc::vec::Vec;

use crate::bits::BitReader;

use super::ps::{Pps, Sps};
use super::{Error, Result};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SliceType {
    P,
    B,
    I,
}

#[derive(Clone, Copy, Debug)]
pub enum RefMod {
    /// modification_of_pic_nums_idc 0 / 1 with abs_diff_pic_num_minus1.
    Sub(u32),
    Add(u32),
    LongTerm(u32),
}

#[derive(Clone, Copy, Debug)]
pub enum Mmco {
    ShortUnused(u32),
    LongUnused(u32),
    ShortToLong(u32, u32),
    MaxLongIdx(u32),
    ClearAll,
    CurrentToLong(u32),
}

#[derive(Clone, Default)]
pub struct Weights {
    pub luma_log2: u32,
    pub chroma_log2: u32,
    /// Per list, per reference index: [luma weight, luma offset, cb w, cb o, cr w, cr o].
    pub table: [Vec<[i32; 6]>; 2],
    /// Whether explicit weights were sent for luma / chroma of each entry.
    pub flags: [Vec<(bool, bool)>; 2],
}

#[derive(Clone)]
pub struct SliceHeader {
    pub first_mb: u32,
    pub slice_type: SliceType,
    pub pps_id: u32,
    pub frame_num: u32,
    pub idr: bool,
    pub nal_ref_idc: u32,
    pub idr_pic_id: u32,
    pub poc_lsb: u32,
    pub delta_poc_bottom: i32,
    pub delta_poc: [i32; 2],
    pub direct_spatial: bool,
    pub num_ref_idx: [u32; 2],
    pub ref_mods: [Vec<RefMod>; 2],
    pub weights: Option<Weights>,
    pub no_output_of_prior_pics: bool,
    pub long_term_reference: bool,
    pub mmcos: Option<Vec<Mmco>>,
    pub cabac_init_idc: u32,
    pub qp: i32,
    pub disable_deblocking: u32,
    pub alpha_offset: i32,
    pub beta_offset: i32,
    /// Bit position where slice_data() starts.
    pub data_bit: usize,
}

impl SliceHeader {
    pub fn has_mmco5(&self) -> bool {
        self.mmcos.as_ref().map(|m| m.iter().any(|x| matches!(x, Mmco::ClearAll))).unwrap_or(false)
    }
}

pub fn peek_pps_id(rbsp: &[u8]) -> Option<(u32, u32)> {
    let mut br = BitReader::new(rbsp);
    let first_mb = br.ue();
    br.ue();
    Some((first_mb, br.ue()))
}

fn ref_mods(br: &mut BitReader) -> Result<Vec<RefMod>> {
    let mut v = Vec::new();
    if br.flag() {
        loop {
            let idc = br.ue();
            match idc {
                0 => v.push(RefMod::Sub(br.ue())),
                1 => v.push(RefMod::Add(br.ue())),
                2 => v.push(RefMod::LongTerm(br.ue())),
                3 => break,
                _ => return Err(Error::Invalid("ref_pic_list_modification")),
            }
            if v.len() > 64 {
                return Err(Error::Invalid("ref_pic_list_modification"));
            }
        }
    }
    Ok(v)
}

fn pred_weight_table(br: &mut BitReader, num: [u32; 2], lists: usize) -> Weights {
    let mut w = Weights { luma_log2: br.ue().min(7), chroma_log2: 0, ..Default::default() };
    w.chroma_log2 = br.ue().min(7);
    for l in 0..lists {
        for _ in 0..num[l] {
            let mut e = [1 << w.luma_log2, 0, 1 << w.chroma_log2, 0, 1 << w.chroma_log2, 0];
            let lf = br.flag();
            if lf {
                e[0] = br.se();
                e[1] = br.se();
            }
            let cf = br.flag();
            if cf {
                e[2] = br.se();
                e[3] = br.se();
                e[4] = br.se();
                e[5] = br.se();
            }
            w.table[l].push(e);
            w.flags[l].push((lf, cf));
        }
    }
    w
}

pub fn parse(rbsp: &[u8], nal_type: u8, nal_ref_idc: u32, sps: &Sps, pps: &Pps) -> Result<SliceHeader> {
    let mut br = BitReader::new(rbsp);
    let first_mb = br.ue();
    let st = br.ue();
    let slice_type = match st % 5 {
        0 => SliceType::P,
        1 => SliceType::B,
        2 => SliceType::I,
        _ => return Err(Error::Unsupported("SP/SI slices")),
    };
    let pps_id = br.ue();
    let frame_num = br.bits(sps.log2_max_frame_num);
    let idr = nal_type == 5;
    let mut h = SliceHeader {
        first_mb,
        slice_type,
        pps_id,
        frame_num,
        idr,
        nal_ref_idc,
        idr_pic_id: 0,
        poc_lsb: 0,
        delta_poc_bottom: 0,
        delta_poc: [0, 0],
        direct_spatial: false,
        num_ref_idx: [0, 0],
        ref_mods: [Vec::new(), Vec::new()],
        weights: None,
        no_output_of_prior_pics: false,
        long_term_reference: false,
        mmcos: None,
        cabac_init_idc: 0,
        qp: 0,
        disable_deblocking: 0,
        alpha_offset: 0,
        beta_offset: 0,
        data_bit: 0,
    };
    if idr {
        h.idr_pic_id = br.ue();
    }
    if sps.poc_type == 0 {
        h.poc_lsb = br.bits(sps.log2_max_poc_lsb);
        if pps.bottom_field_pic_order_in_frame_present {
            h.delta_poc_bottom = br.se();
        }
    }
    if sps.poc_type == 1 && !sps.delta_pic_order_always_zero {
        h.delta_poc[0] = br.se();
        if pps.bottom_field_pic_order_in_frame_present {
            h.delta_poc[1] = br.se();
        }
    }
    if pps.redundant_pic_cnt_present && br.ue() > 0 {
        return Err(Error::Unsupported("redundant slice"));
    }
    if slice_type == SliceType::B {
        h.direct_spatial = br.flag();
    }
    if slice_type != SliceType::I {
        h.num_ref_idx = pps.num_ref_idx_default;
        if br.flag() {
            h.num_ref_idx[0] = br.ue() + 1;
            if slice_type == SliceType::B {
                h.num_ref_idx[1] = br.ue() + 1;
            }
        }
        if slice_type != SliceType::B {
            h.num_ref_idx[1] = 0;
        }
        if h.num_ref_idx[0] > 32 || h.num_ref_idx[1] > 32 {
            return Err(Error::Invalid("num_ref_idx_active"));
        }
        h.ref_mods[0] = ref_mods(&mut br)?;
        if slice_type == SliceType::B {
            h.ref_mods[1] = ref_mods(&mut br)?;
        }
    }
    if (pps.weighted_pred && slice_type == SliceType::P) || (pps.weighted_bipred_idc == 1 && slice_type == SliceType::B) {
        let lists = if slice_type == SliceType::B { 2 } else { 1 };
        h.weights = Some(pred_weight_table(&mut br, h.num_ref_idx, lists));
    }
    if nal_ref_idc != 0 {
        if idr {
            h.no_output_of_prior_pics = br.flag();
            h.long_term_reference = br.flag();
        } else if br.flag() {
            let mut v = Vec::new();
            loop {
                let op = br.ue();
                match op {
                    0 => break,
                    1 => v.push(Mmco::ShortUnused(br.ue())),
                    2 => v.push(Mmco::LongUnused(br.ue())),
                    3 => {
                        let d = br.ue();
                        v.push(Mmco::ShortToLong(d, br.ue()));
                    }
                    4 => v.push(Mmco::MaxLongIdx(br.ue())),
                    5 => v.push(Mmco::ClearAll),
                    6 => v.push(Mmco::CurrentToLong(br.ue())),
                    _ => return Err(Error::Invalid("mmco")),
                }
                if v.len() > 66 {
                    return Err(Error::Invalid("mmco"));
                }
            }
            h.mmcos = Some(v);
        }
    }
    if pps.cabac && slice_type != SliceType::I {
        h.cabac_init_idc = br.ue();
        if h.cabac_init_idc > 2 {
            return Err(Error::Invalid("cabac_init_idc"));
        }
    }
    h.qp = pps.pic_init_qp + br.se();
    if !(0..=51).contains(&h.qp) {
        return Err(Error::Invalid("slice qp"));
    }
    if pps.deblocking_filter_control_present {
        h.disable_deblocking = br.ue();
        if h.disable_deblocking > 2 {
            return Err(Error::Invalid("disable_deblocking_filter_idc"));
        }
        if h.disable_deblocking != 1 {
            h.alpha_offset = br.se() * 2;
            h.beta_offset = br.se() * 2;
        }
    }
    if br.bits_left() < 0 {
        return Err(Error::Invalid("slice header truncated"));
    }
    h.data_bit = br.position();
    Ok(h)
}
