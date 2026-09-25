//! Shared picture and macroblock state.

use alloc::sync::Arc;
use alloc::vec::Vec;

pub const KIND_I4: u8 = 0;
pub const KIND_I8: u8 = 1;
pub const KIND_I16: u8 = 2;
pub const KIND_PCM: u8 = 3;
pub const KIND_PSKIP: u8 = 4;
pub const KIND_BSKIP: u8 = 5;
pub const KIND_INTER: u8 = 6;

pub const NO_REF: u32 = u32::MAX;

/// Per-macroblock state kept for the whole picture (neighbour contexts,
/// deblocking) and, in reduced form, for co-located lookups.
#[derive(Clone, Copy)]
pub struct MbInfo {
    /// Slice number + 1; 0 means not decoded yet.
    pub slice: u32,
    pub kind: u8,
    /// B_Skip or B_Direct_16x16.
    pub direct16: bool,
    pub qp: u8,
    pub cbp: u8,
    pub t8x8: bool,
    pub intra_modes: [u8; 16],
    pub chroma_mode: u8,
    /// Non-zero coefficient counts: luma 4x4 raster, then Cb 2x2, Cr 2x2.
    pub nz: [u8; 24],
    /// coded_block_flag of DC blocks: bit 0 luma, 1 Cb, 2 Cr.
    pub cbf_dc: u8,
    pub mv: [[[i16; 2]; 16]; 2],
    pub ref_idx: [[i8; 4]; 2],
    pub ref_id: [[u32; 4]; 2],
    pub mvd: [[[u8; 2]; 16]; 2],
    /// Bit per 8x8: predicted in direct mode.
    pub direct8: u8,
}

impl MbInfo {
    pub const EMPTY: MbInfo = MbInfo {
        slice: 0,
        kind: KIND_INTER,
        direct16: false,
        qp: 0,
        cbp: 0,
        t8x8: false,
        intra_modes: [2; 16],
        chroma_mode: 0,
        nz: [0; 24],
        cbf_dc: 0,
        mv: [[[0; 2]; 16]; 2],
        ref_idx: [[-1; 4]; 2],
        ref_id: [[NO_REF; 4]; 2],
        mvd: [[[0; 2]; 16]; 2],
        direct8: 0,
    };

    #[inline]
    pub fn is_intra(&self) -> bool {
        self.kind <= KIND_PCM
    }

    #[inline]
    pub fn is_skip(&self) -> bool {
        self.kind == KIND_PSKIP || self.kind == KIND_BSKIP
    }
}

/// A decoded picture (planes sized to whole macroblocks).
pub struct PicBuf {
    pub id: u32,
    pub poc: i32,
    pub width: usize,
    pub height: usize,
    pub y: Vec<u8>,
    pub cb: Vec<u8>,
    pub cr: Vec<u8>,
    /// Macroblock data, kept for co-located (direct mode) lookups.
    pub info: Vec<MbInfo>,
}

#[derive(Clone)]
pub struct RefPic {
    pub buf: Arc<PicBuf>,
    pub long_term: bool,
}

impl RefPic {
    pub fn id(&self) -> u32 {
        self.buf.id
    }
    pub fn poc(&self) -> i32 {
        self.buf.poc
    }
}

/// Deblocking parameters of a slice.
#[derive(Clone, Copy)]
pub struct SliceParams {
    pub disable: u32,
    pub alpha: i32,
    pub beta: i32,
    pub cqo: [i32; 2],
}

/// The picture currently being decoded.
pub struct CurPic {
    pub mb_w: usize,
    pub mb_h: usize,
    pub y: Vec<u8>,
    pub cb: Vec<u8>,
    pub cr: Vec<u8>,
    pub info: Vec<MbInfo>,
    pub slices: Vec<SliceParams>,
    pub decoded: usize,
    pub poc: i32,
    pub id: u32,
}

impl CurPic {
    /// A new picture, reusing the buffers of `old` when given.
    pub fn new(mb_w: usize, mb_h: usize, poc: i32, id: u32, old: Option<PicBuf>) -> CurPic {
        let n = mb_w * mb_h;
        let (y, cb, cr, mut info) = match old {
            Some(o) if o.info.len() == n => (o.y, o.cb, o.cr, o.info),
            _ => (alloc::vec![0; n * 256], alloc::vec![128; n * 64], alloc::vec![128; n * 64], alloc::vec![MbInfo::EMPTY; n]),
        };
        for i in info.iter_mut() {
            i.slice = 0;
        }
        CurPic { mb_w, mb_h, y, cb, cr, info, slices: Vec::new(), decoded: 0, poc, id }
    }
}

pub const CHROMA_QP: [u8; 52] = [
    0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 29, 30, 31,
    32, 32, 33, 34, 34, 35, 35, 36, 36, 37, 37, 37, 38, 38, 38, 39, 39, 39, 39,
];

pub fn chroma_qp(qp: i32, offset: i32) -> i32 {
    CHROMA_QP[(qp + offset).clamp(0, 51) as usize] as i32
}
