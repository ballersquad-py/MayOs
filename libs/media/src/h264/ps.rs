//! Sequence and picture parameter sets (7.3.2.1, 7.3.2.2).

use alloc::vec::Vec;

use crate::bits::BitReader;
use crate::tables::{DEFAULT_SCALING4, DEFAULT_SCALING8};

use super::{Error, Result, ZIGZAG4, ZIGZAG8};

#[derive(Clone)]
pub struct Sps {
    pub profile_idc: u8,
    pub level_idc: u8,
    pub id: u32,
    pub chroma_format_idc: u32,
    pub scaling4: [[u8; 16]; 6],
    pub scaling8: [[u8; 64]; 2],
    /// seq_scaling_matrix_present_flag (selects the PPS fall-back rule).
    pub has_matrix: bool,
    pub log2_max_frame_num: u32,
    pub poc_type: u32,
    pub log2_max_poc_lsb: u32,
    pub delta_pic_order_always_zero: bool,
    pub offset_for_non_ref_pic: i32,
    pub offset_for_top_to_bottom_field: i32,
    pub offset_for_ref_frame: Vec<i32>,
    pub max_num_ref_frames: u32,
    pub gaps_allowed: bool,
    pub width_mbs: u32,
    pub height_mbs: u32,
    pub frame_mbs_only: bool,
    pub direct_8x8_inference: bool,
    /// Crop in luma samples: left, right, top, bottom.
    pub crop: [u32; 4],
    pub num_reorder_frames: Option<u32>,
    pub max_dec_frame_buffering: Option<u32>,
    /// Frames per second as (num_units_in_tick, time_scale) if signalled.
    pub timing: Option<(u32, u32)>,
    /// Full range (0-255) video rather than 16-235.
    pub full_range: bool,
    /// Colour matrix: 1 = BT.709, 6 = BT.601 (default when unknown).
    pub matrix: u8,
}

#[derive(Clone)]
pub struct Pps {
    pub id: u32,
    pub sps_id: u32,
    pub cabac: bool,
    pub bottom_field_pic_order_in_frame_present: bool,
    pub num_ref_idx_default: [u32; 2],
    pub weighted_pred: bool,
    pub weighted_bipred_idc: u32,
    pub pic_init_qp: i32,
    pub chroma_qp_offset: [i32; 2],
    pub deblocking_filter_control_present: bool,
    pub constrained_intra_pred: bool,
    pub redundant_pic_cnt_present: bool,
    pub transform_8x8_mode: bool,
    /// Scaling lists in raster order: 4x4 [Y intra, Cb intra, Cr intra, Y inter, Cb inter, Cr inter].
    pub scaling4: [[u8; 16]; 6],
    pub scaling8: [[u8; 64]; 2],
}

/// Parse one scaling list; returns the list in zigzag order and whether the
/// default list should be used.
fn scaling_list(br: &mut BitReader, out: &mut [u8]) -> bool {
    let mut last = 8i32;
    let mut next = 8i32;
    for j in 0..out.len() {
        if next != 0 {
            let delta = br.se();
            next = (last + delta + 256) % 256;
            if j == 0 && next == 0 {
                return true;
            }
        }
        out[j] = if next == 0 { last as u8 } else { next as u8 };
        last = out[j] as i32;
    }
    false
}

/// Default lists (Table 7-3/7-4) in zigzag order; the table is raster.
fn default4(i: usize) -> [u8; 16] {
    let t = &DEFAULT_SCALING4[(if i < 3 { 0 } else { 1 }) * 16..][..16];
    let mut o = [0u8; 16];
    for k in 0..16 {
        o[k] = t[ZIGZAG4[k] as usize];
    }
    o
}

fn default8(i: usize) -> [u8; 64] {
    let t = &DEFAULT_SCALING8[i * 64..][..64];
    let mut o = [0u8; 64];
    for k in 0..64 {
        o[k] = t[ZIGZAG8[k] as usize];
    }
    o
}

/// Scaling matrices in zigzag order, applying fall-back rules A (SPS) or B
/// (PPS, falling back to the SPS lists).
fn scaling_matrices(
    br: &mut BitReader,
    n8: usize,
    fallback: Option<(&[[u8; 16]; 6], &[[u8; 64]; 2])>,
) -> ([[u8; 16]; 6], [[u8; 64]; 2]) {
    let mut s4 = [[16u8; 16]; 6];
    let mut s8 = [[16u8; 64]; 2];
    for i in 0..6 {
        if br.flag() {
            if scaling_list(br, &mut s4[i]) {
                s4[i] = default4(i);
            }
        } else {
            s4[i] = match (i, fallback) {
                (0, None) | (3, None) => default4(i),
                (0, Some((f4, _))) | (3, Some((f4, _))) => f4[i],
                _ => s4[i - 1],
            };
        }
    }
    for i in 0..n8 {
        if br.flag() {
            if scaling_list(br, &mut s8[i]) {
                s8[i] = default8(i);
            }
        } else {
            s8[i] = match fallback {
                None => default8(i),
                Some((_, f8)) => f8[i],
            };
        }
    }
    (s4, s8)
}

fn to_raster4(z: &[[u8; 16]; 6]) -> [[u8; 16]; 6] {
    let mut o = [[0u8; 16]; 6];
    for l in 0..6 {
        for k in 0..16 {
            o[l][ZIGZAG4[k] as usize] = z[l][k];
        }
    }
    o
}

fn to_raster8(z: &[[u8; 64]; 2]) -> [[u8; 64]; 2] {
    let mut o = [[0u8; 64]; 2];
    for l in 0..2 {
        for k in 0..64 {
            o[l][ZIGZAG8[k] as usize] = z[l][k];
        }
    }
    o
}

fn to_zigzag4(r: &[[u8; 16]; 6]) -> [[u8; 16]; 6] {
    let mut o = [[0u8; 16]; 6];
    for l in 0..6 {
        for k in 0..16 {
            o[l][k] = r[l][ZIGZAG4[k] as usize];
        }
    }
    o
}

fn to_zigzag8(r: &[[u8; 64]; 2]) -> [[u8; 64]; 2] {
    let mut o = [[0u8; 64]; 2];
    for l in 0..2 {
        for k in 0..64 {
            o[l][k] = r[l][ZIGZAG8[k] as usize];
        }
    }
    o
}

fn hrd_parameters(br: &mut BitReader) {
    let cpb_cnt = br.ue() + 1;
    br.bits(4);
    br.bits(4);
    for _ in 0..cpb_cnt.min(32) {
        br.ue();
        br.ue();
        br.flag();
    }
    br.bits(5);
    br.bits(5);
    br.bits(5);
    br.bits(5);
}

pub fn parse_sps(rbsp: &[u8]) -> Result<Sps> {
    let mut br = BitReader::new(rbsp);
    let profile_idc = br.bits(8) as u8;
    br.bits(8); // constraint flags
    let level_idc = br.bits(8) as u8;
    let id = br.ue();
    if id > 31 {
        return Err(Error::Invalid("sps id"));
    }
    let mut chroma_format_idc = 1;
    let mut s4 = [[16u8; 16]; 6];
    let mut s8 = [[16u8; 64]; 2];
    let mut has_matrix = false;
    if matches!(profile_idc, 100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135) {
        chroma_format_idc = br.ue();
        if chroma_format_idc == 3 {
            br.flag(); // separate_colour_plane_flag
        }
        let bit_depth_luma = br.ue() + 8;
        let bit_depth_chroma = br.ue() + 8;
        if bit_depth_luma != 8 || bit_depth_chroma != 8 {
            return Err(Error::Unsupported("only 8-bit video is supported"));
        }
        if br.flag() {
            return Err(Error::Unsupported("lossless (transform bypass) video"));
        }
        if br.flag() {
            has_matrix = true;
            let n8 = if chroma_format_idc == 3 { 6 } else { 2 };
            let (z4, z8) = scaling_matrices(&mut br, n8.min(2), None);
            if n8 > 2 {
                // 4:4:4 lists for chroma 8x8 blocks are parsed and ignored.
                let mut tmp = [0u8; 64];
                for _ in 2..n8 {
                    if br.flag() {
                        scaling_list(&mut br, &mut tmp);
                    }
                }
            }
            s4 = to_raster4(&z4);
            s8 = to_raster8(&z8);
        }
    }
    if chroma_format_idc != 1 {
        return Err(Error::Unsupported("only 4:2:0 video is supported"));
    }
    let log2_max_frame_num = br.ue() + 4;
    let poc_type = br.ue();
    let mut log2_max_poc_lsb = 0;
    let mut delta_pic_order_always_zero = false;
    let mut offset_for_non_ref_pic = 0;
    let mut offset_for_top_to_bottom_field = 0;
    let mut offset_for_ref_frame = Vec::new();
    match poc_type {
        0 => log2_max_poc_lsb = br.ue() + 4,
        1 => {
            delta_pic_order_always_zero = br.flag();
            offset_for_non_ref_pic = br.se();
            offset_for_top_to_bottom_field = br.se();
            let n = br.ue();
            if n > 255 {
                return Err(Error::Invalid("poc cycle"));
            }
            for _ in 0..n {
                offset_for_ref_frame.push(br.se());
            }
        }
        2 => {}
        _ => return Err(Error::Invalid("poc type")),
    }
    let max_num_ref_frames = br.ue();
    let gaps_allowed = br.flag();
    let width_mbs = br.ue() + 1;
    let height_map_units = br.ue() + 1;
    let frame_mbs_only = br.flag();
    if !frame_mbs_only {
        return Err(Error::Unsupported("interlaced video"));
    }
    let height_mbs = height_map_units;
    if width_mbs > 512 || height_mbs > 512 {
        return Err(Error::Unsupported("picture too large"));
    }
    let direct_8x8_inference = br.flag();
    let mut crop = [0u32; 4];
    if br.flag() {
        for c in crop.iter_mut() {
            *c = br.ue() * 2;
        }
    }
    let mut sps = Sps {
        profile_idc,
        level_idc,
        id,
        chroma_format_idc,
        scaling4: s4,
        scaling8: s8,
        has_matrix,
        log2_max_frame_num,
        poc_type,
        log2_max_poc_lsb,
        delta_pic_order_always_zero,
        offset_for_non_ref_pic,
        offset_for_top_to_bottom_field,
        offset_for_ref_frame,
        max_num_ref_frames,
        gaps_allowed,
        width_mbs,
        height_mbs,
        frame_mbs_only,
        direct_8x8_inference,
        crop,
        num_reorder_frames: None,
        max_dec_frame_buffering: None,
        timing: None,
        full_range: false,
        matrix: 0,
    };
    if br.flag() {
        // VUI
        if br.flag() {
            let idc = br.bits(8);
            if idc == 255 {
                br.bits(16);
                br.bits(16);
            }
        }
        if br.flag() {
            br.flag();
        }
        if br.flag() {
            br.bits(3);
            sps.full_range = br.flag();
            if br.flag() {
                br.bits(8);
                br.bits(8);
                sps.matrix = br.bits(8) as u8;
            }
        }
        if br.flag() {
            br.ue();
            br.ue();
        }
        if br.flag() {
            let units = br.bits(32);
            let scale = br.bits(32);
            br.flag();
            if units > 0 && scale > 0 {
                sps.timing = Some((units, scale));
            }
        }
        let nal_hrd = br.flag();
        if nal_hrd {
            hrd_parameters(&mut br);
        }
        let vcl_hrd = br.flag();
        if vcl_hrd {
            hrd_parameters(&mut br);
        }
        if nal_hrd || vcl_hrd {
            br.flag();
        }
        br.flag(); // pic_struct_present
        if br.flag() {
            br.flag();
            br.ue();
            br.ue();
            br.ue();
            br.ue();
            let reorder = br.ue();
            let max_dec = br.ue();
            if br.bits_left() >= 0 && reorder <= 16 && max_dec <= 16 {
                sps.num_reorder_frames = Some(reorder);
                sps.max_dec_frame_buffering = Some(max_dec.max(1));
            }
        }
    }
    Ok(sps)
}

pub fn parse_pps(rbsp: &[u8], sps_list: &[Option<Sps>]) -> Result<Pps> {
    let mut br = BitReader::new(rbsp);
    let id = br.ue();
    let sps_id = br.ue();
    if id > 255 || sps_id > 31 {
        return Err(Error::Invalid("pps id"));
    }
    let sps = sps_list.get(sps_id as usize).and_then(|s| s.as_ref()).ok_or(Error::Invalid("pps refers to missing sps"))?;
    let cabac = br.flag();
    let bottom_field_pic_order_in_frame_present = br.flag();
    let num_slice_groups = br.ue() + 1;
    if num_slice_groups > 1 {
        return Err(Error::Unsupported("slice groups (FMO)"));
    }
    let num_ref_idx_default = [br.ue() + 1, br.ue() + 1];
    if num_ref_idx_default[0] > 32 || num_ref_idx_default[1] > 32 {
        return Err(Error::Invalid("num_ref_idx"));
    }
    let weighted_pred = br.flag();
    let weighted_bipred_idc = br.bits(2);
    let pic_init_qp = 26 + br.se();
    let _pic_init_qs = 26 + br.se();
    let cqo = br.se();
    let deblocking_filter_control_present = br.flag();
    let constrained_intra_pred = br.flag();
    let redundant_pic_cnt_present = br.flag();
    let mut pps = Pps {
        id,
        sps_id,
        cabac,
        bottom_field_pic_order_in_frame_present,
        num_ref_idx_default,
        weighted_pred,
        weighted_bipred_idc,
        pic_init_qp,
        chroma_qp_offset: [cqo, cqo],
        deblocking_filter_control_present,
        constrained_intra_pred,
        redundant_pic_cnt_present,
        transform_8x8_mode: false,
        scaling4: sps.scaling4,
        scaling8: sps.scaling8,
    };
    if br.more_rbsp_data() {
        pps.transform_8x8_mode = br.flag();
        if br.flag() {
            let n8 = if pps.transform_8x8_mode { 2 } else { 0 };
            let f4 = to_zigzag4(&sps.scaling4);
            let f8 = to_zigzag8(&sps.scaling8);
            let fallback = if sps.has_matrix { Some((&f4, &f8)) } else { None };
            let (z4, z8) = scaling_matrices(&mut br, n8, fallback);
            pps.scaling4 = to_raster4(&z4);
            if n8 > 0 {
                pps.scaling8 = to_raster8(&z8);
            }
        }
        pps.chroma_qp_offset[1] = br.se();
    }
    Ok(pps)
}
