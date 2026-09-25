//! Deblocking filter (8.7).

use crate::tables::{DEBLOCK_ALPHA, DEBLOCK_BETA, DEBLOCK_TC0};

use super::transform::clip_u8;
use super::types::*;

fn nonzero(i: &MbInfo, blk: usize) -> bool {
    if i.t8x8 {
        let (qx, qy) = ((blk % 4) / 2 * 2, (blk / 4) / 2 * 2);
        i.nz[qy * 4 + qx] | i.nz[qy * 4 + qx + 1] | i.nz[qy * 4 + qx + 4] | i.nz[qy * 4 + qx + 5] != 0
    } else {
        i.nz[blk] != 0
    }
}

fn mv_far(a: [i16; 2], b: [i16; 2]) -> bool {
    (a[0] as i32 - b[0] as i32).abs() >= 4 || (a[1] as i32 - b[1] as i32).abs() >= 4
}

#[inline(always)]
fn strength(p: &MbInfo, pb: usize, q: &MbInfo, qb: usize, mb_edge: bool) -> u8 {
    if p.is_intra() || q.is_intra() {
        return if mb_edge { 4 } else { 3 };
    }
    if nonzero(p, pb) || nonzero(q, qb) {
        return 2;
    }
    let p8 = (pb / 8) * 2 + (pb % 4) / 2;
    let q8 = (qb / 8) * 2 + (qb % 4) / 2;
    let pr = [if p.ref_idx[0][p8] >= 0 { p.ref_id[0][p8] } else { NO_REF }, if p.ref_idx[1][p8] >= 0 { p.ref_id[1][p8] } else { NO_REF }];
    let qr = [if q.ref_idx[0][q8] >= 0 { q.ref_id[0][q8] } else { NO_REF }, if q.ref_idx[1][q8] >= 0 { q.ref_id[1][q8] } else { NO_REF }];
    let pn = (pr[0] != NO_REF) as u8 + (pr[1] != NO_REF) as u8;
    let qn = (qr[0] != NO_REF) as u8 + (qr[1] != NO_REF) as u8;
    if pn != qn {
        return 1;
    }
    let pm = [p.mv[0][pb], p.mv[1][pb]];
    let qm = [q.mv[0][qb], q.mv[1][qb]];
    if pn == 1 {
        let (pi, qi) = (if pr[0] != NO_REF { 0 } else { 1 }, if qr[0] != NO_REF { 0 } else { 1 });
        if pr[pi] != qr[qi] {
            return 1;
        }
        return mv_far(pm[pi], qm[qi]) as u8;
    }
    if pn == 0 {
        return 0;
    }
    // Two motion vectors each.
    let same_set = (pr[0] == qr[0] && pr[1] == qr[1]) || (pr[0] == qr[1] && pr[1] == qr[0]);
    if !same_set {
        return 1;
    }
    if pr[0] != pr[1] {
        if pr[0] == qr[0] {
            (mv_far(pm[0], qm[0]) || mv_far(pm[1], qm[1])) as u8
        } else {
            (mv_far(pm[0], qm[1]) || mv_far(pm[1], qm[0])) as u8
        }
    } else {
        ((mv_far(pm[0], qm[0]) || mv_far(pm[1], qm[1])) && (mv_far(pm[0], qm[1]) || mv_far(pm[1], qm[0]))) as u8
    }
}

/// Filter one line of samples across an edge. `o` is the index of q0,
/// `d` the step from q0 to q1.
#[inline(always)]
fn filter_line(s: &mut [u8], o: usize, d: isize, bs: u8, alpha: i32, beta: i32, tc0: i32, chroma: bool) {
    let at = |k: isize| (o as isize + k * d) as usize;
    let p0 = s[at(-1)] as i32;
    let q0 = s[at(0)] as i32;
    let p1 = s[at(-2)] as i32;
    let q1 = s[at(1)] as i32;
    if (p0 - q0).abs() >= alpha || (p1 - p0).abs() >= beta || (q1 - q0).abs() >= beta {
        return;
    }
    if chroma {
        if bs < 4 {
            let tc = tc0 + 1;
            let delta = ((((q0 - p0) << 2) + (p1 - q1) + 4) >> 3).clamp(-tc, tc);
            s[at(-1)] = clip_u8(p0 + delta);
            s[at(0)] = clip_u8(q0 - delta);
        } else {
            s[at(-1)] = ((2 * p1 + p0 + q1 + 2) >> 2) as u8;
            s[at(0)] = ((2 * q1 + q0 + p1 + 2) >> 2) as u8;
        }
        return;
    }
    let p2 = s[at(-3)] as i32;
    let q2 = s[at(2)] as i32;
    let ap = (p2 - p0).abs();
    let aq = (q2 - q0).abs();
    if bs < 4 {
        let tc = tc0 + (ap < beta) as i32 + (aq < beta) as i32;
        let delta = ((((q0 - p0) << 2) + (p1 - q1) + 4) >> 3).clamp(-tc, tc);
        s[at(-1)] = clip_u8(p0 + delta);
        s[at(0)] = clip_u8(q0 - delta);
        if ap < beta {
            s[at(-2)] = (p1 + ((p2 + ((p0 + q0 + 1) >> 1) - (p1 << 1)) >> 1).clamp(-tc0, tc0)) as u8;
        }
        if aq < beta {
            s[at(1)] = (q1 + ((q2 + ((p0 + q0 + 1) >> 1) - (q1 << 1)) >> 1).clamp(-tc0, tc0)) as u8;
        }
    } else {
        let strong = (p0 - q0).abs() < ((alpha >> 2) + 2);
        if ap < beta && strong {
            let p3 = s[at(-4)] as i32;
            s[at(-1)] = ((p2 + 2 * p1 + 2 * p0 + 2 * q0 + q1 + 4) >> 3) as u8;
            s[at(-2)] = ((p2 + p1 + p0 + q0 + 2) >> 2) as u8;
            s[at(-3)] = ((2 * p3 + 3 * p2 + p1 + p0 + q0 + 4) >> 3) as u8;
        } else {
            s[at(-1)] = ((2 * p1 + p0 + q1 + 2) >> 2) as u8;
        }
        if aq < beta && strong {
            let q3 = s[at(3)] as i32;
            s[at(0)] = ((p1 + 2 * p0 + 2 * q0 + 2 * q1 + q2 + 4) >> 3) as u8;
            s[at(1)] = ((p0 + q0 + q1 + q2 + 2) >> 2) as u8;
            s[at(2)] = ((2 * q3 + 3 * q2 + q1 + q0 + p0 + 4) >> 3) as u8;
        } else {
            s[at(0)] = ((2 * q1 + q0 + p1 + 2) >> 2) as u8;
        }
    }
}

struct EdgeParams {
    alpha: i32,
    beta: i32,
    tc0: [i32; 4],
}

fn params(qp_av: i32, sp: &SliceParams, bs: &[u8; 4]) -> EdgeParams {
    let ia = (qp_av + sp.alpha).clamp(0, 51) as usize;
    let ib = (qp_av + sp.beta).clamp(0, 51) as usize;
    let mut tc0 = [0i32; 4];
    for k in 0..4 {
        if bs[k] > 0 && bs[k] < 4 {
            tc0[k] = DEBLOCK_TC0[ia * 3 + bs[k] as usize - 1] as i32;
        }
    }
    EdgeParams { alpha: DEBLOCK_ALPHA[ia] as i32, beta: DEBLOCK_BETA[ib] as i32, tc0 }
}

/// True when the macroblock's internal edges all have bS 0: inter, no
/// luma coefficients and the same motion everywhere.
fn uniform(i: &MbInfo) -> bool {
    if i.is_intra() || i.cbp & 15 != 0 {
        return false;
    }
    if i.nz[..16].iter().any(|&n| n != 0) {
        return false;
    }
    let r = (i.ref_id[0][0], i.ref_id[1][0], i.ref_idx[0][0] >= 0, i.ref_idx[1][0] >= 0);
    for b8 in 1..4 {
        if (i.ref_id[0][b8], i.ref_id[1][b8], i.ref_idx[0][b8] >= 0, i.ref_idx[1][b8] >= 0) != r {
            return false;
        }
    }
    let m0 = i.mv[0][0];
    let m1 = i.mv[1][0];
    i.mv[0].iter().all(|&m| m == m0) && i.mv[1].iter().all(|&m| m == m1)
}

pub fn deblock_picture(pic: &mut CurPic) {
    deblock_all(pic)
}

fn deblock_all(pic: &mut CurPic) {
    let (mb_w, mb_h) = (pic.mb_w, pic.mb_h);
    let ys = mb_w * 16;
    let cs = mb_w * 8;
    let CurPic { info, slices, y: py, cb: pcb, cr: pcr, .. } = pic;
    for mb in 0..mb_w * mb_h {
        let q = &info[mb];
        if q.slice == 0 {
            continue;
        }
        let sp = slices[q.slice as usize - 1];
        if sp.disable == 1 {
            continue;
        }
        let (mx, my) = (mb % mb_w, mb / mb_w);
        let internal_zero = uniform(q);
        for dir in 0..2 {
            // dir 0: vertical edges (filter horizontally), 1: horizontal edges.
            let nb = if dir == 0 {
                if mx > 0 { Some(mb - 1) } else { None }
            } else if my > 0 {
                Some(mb - mb_w)
            } else {
                None
            };
            let nb = nb.filter(|&n| info[n].slice != 0 && (sp.disable != 2 || info[n].slice == q.slice));
            for e in 0..4 {
                if e == 0 && nb.is_none() {
                    continue;
                }
                if e > 0 && (internal_zero || (e % 2 == 1 && q.t8x8)) {
                    continue;
                }
                let p_info = if e == 0 { &info[nb.unwrap()] } else { q };
                let mut bs = [0u8; 4];
                for k in 0..4 {
                    let (qb, pb) = if dir == 0 {
                        (k * 4 + e, if e == 0 { k * 4 + 3 } else { k * 4 + e - 1 })
                    } else {
                        (e * 4 + k, if e == 0 { 12 + k } else { (e - 1) * 4 + k })
                    };
                    bs[k] = strength(p_info, pb, q, qb, e == 0);
                }
                if bs == [0; 4] {
                    continue;
                }
                // Luma.
                let qp_av = (p_info.qp as i32 + q.qp as i32 + 1) >> 1;
                let ep = params(qp_av, &sp, &bs);
                for k in 0..16 {
                    let b = bs[k / 4];
                    if b == 0 {
                        continue;
                    }
                    let (o, d) = if dir == 0 {
                        ((my * 16 + k) * ys + mx * 16 + e * 4, 1isize)
                    } else {
                        ((my * 16 + e * 4) * ys + mx * 16 + k, ys as isize)
                    };
                    filter_line(py, o, d, b, ep.alpha, ep.beta, ep.tc0[k / 4], false);
                }
                // Chroma edges at 0 and 2 (chroma sample 0 and 4).
                if e % 2 == 0 {
                    for c in 0..2 {
                        let qpc_p = chroma_qp(p_info.qp as i32, sp.cqo[c]);
                        let qpc_q = chroma_qp(q.qp as i32, sp.cqo[c]);
                        let ep = params((qpc_p + qpc_q + 1) >> 1, &sp, &bs);
                        let plane: &mut [u8] = if c == 0 { pcb } else { pcr };
                        for k in 0..8 {
                            let b = bs[k / 2];
                            if b == 0 {
                                continue;
                            }
                            let (o, d) = if dir == 0 {
                                ((my * 8 + k) * cs + mx * 8 + e * 2, 1isize)
                            } else {
                                ((my * 8 + e * 2) * cs + mx * 8 + k, cs as isize)
                            };
                            filter_line(plane, o, d, b, ep.alpha, ep.beta, ep.tc0[k / 2], true);
                        }
                    }
                }
            }
        }
    }
}
