//! Fractional-sample interpolation for motion compensation (8.4.2.2).

use super::transform::clip_u8;

const WS: usize = 24; // window stride

#[inline(always)]
fn tap(r: &[u8]) -> i32 {
    r[0] as i32 - 5 * r[1] as i32 + 20 * r[2] as i32 + 20 * r[3] as i32 - 5 * r[4] as i32 + r[5] as i32
}

#[inline(always)]
fn tap_i(a: i32, b: i32, c: i32, d: i32, e: i32, f: i32) -> i32 {
    a - 5 * (b + e) + 20 * (c + d) + f
}

/// Vertical half samples (rounded, clipped) for one output row: column
/// base `c0` is the sample 2 rows above the output row.
#[inline(always)]
fn vhalf_row(s: &[u8], st: usize, c0: usize, w: usize, out: &mut [i32; 16]) {
    let r0 = &s[c0..c0 + w];
    let r1 = &s[c0 + st..c0 + st + w];
    let r2 = &s[c0 + 2 * st..c0 + 2 * st + w];
    let r3 = &s[c0 + 3 * st..c0 + 3 * st + w];
    let r4 = &s[c0 + 4 * st..c0 + 4 * st + w];
    let r5 = &s[c0 + 5 * st..c0 + 5 * st + w];
    for i in 0..w {
        let t = tap_i(r0[i] as i32, r1[i] as i32, r2[i] as i32, r3[i] as i32, r4[i] as i32, r5[i] as i32);
        out[i] = clip_u8((t + 16) >> 5) as i32;
    }
}

/// Source view: G(i, j) = s[o + j * st + i], valid for i in -2..w+3 and
/// j in -2..h+3.
struct Src<'a> {
    s: &'a [u8],
    st: usize,
    o: usize,
}

/// Luma prediction of a w x h block whose integer position is (x, y) with
/// quarter-sample fraction (fx, fy); result in `out` (stride 16).
#[allow(clippy::too_many_arguments)]
#[inline]
pub fn luma(src: &[u8], stride: usize, pw: usize, ph: usize, x: i32, y: i32, fx: u32, fy: u32, w: usize, h: usize, out: &mut [u8], oo: usize, os: usize) {
    luma_impl(src, stride, pw, ph, x, y, fx, fy, w, h, out, oo, os)
}

#[allow(clippy::too_many_arguments)]
fn luma_impl(src: &[u8], stride: usize, pw: usize, ph: usize, x: i32, y: i32, fx: u32, fy: u32, w: usize, h: usize, out: &mut [u8], oo: usize, os: usize) {
    let mut win = [0u8; WS * WS];
    let inside = x >= 2 && y >= 2 && x as usize + w + 3 <= pw && y as usize + h + 3 <= ph;
    let v = if inside {
        Src { s: src, st: stride, o: y as usize * stride + x as usize }
    } else {
        for r in 0..h + 5 {
            let sy = (y - 2 + r as i32).clamp(0, ph as i32 - 1) as usize;
            let row = &src[sy * stride..sy * stride + pw];
            let dst = &mut win[r * WS..r * WS + w + 5];
            for (c, d) in dst.iter_mut().enumerate() {
                *d = row[(x - 2 + c as i32).clamp(0, pw as i32 - 1) as usize];
            }
        }
        Src { s: &win, st: WS, o: 2 * WS + 2 }
    };
    let row = |j: isize| -> usize { (v.o as isize + j * v.st as isize) as usize };
    match (fx, fy) {
        (0, 0) => {
            for j in 0..h {
                let r = row(j as isize);
                out[oo + j * os..oo + j * os + w].copy_from_slice(&v.s[r..r + w]);
            }
        }
        (_, 0) => {
            for j in 0..h {
                let r = row(j as isize);
                let line = &v.s[r - 2..r + w + 3];
                let o = &mut out[oo + j * os..oo + j * os + w];
                for (i, (d, win6)) in o.iter_mut().zip(line.windows(6)).enumerate() {
                    let b = clip_u8((tap(win6) + 16) >> 5) as i32;
                    *d = match fx {
                        1 => ((line[i + 2] as i32 + b + 1) >> 1) as u8,
                        2 => b as u8,
                        _ => ((line[i + 3] as i32 + b + 1) >> 1) as u8,
                    };
                }
            }
        }
        (0, _) => {
            let st = v.st;
            let mut hv = [0i32; 16];
            for j in 0..h {
                let r = row(j as isize - 2);
                vhalf_row(v.s, st, r, w, &mut hv);
                let g = if fy == 1 { r + 2 * st } else { r + 3 * st };
                let gs = &v.s[g..g + w];
                let o = &mut out[oo + j * os..oo + j * os + w];
                if fy == 2 {
                    for i in 0..w {
                        o[i] = hv[i] as u8;
                    }
                } else {
                    for i in 0..w {
                        o[i] = ((gs[i] as i32 + hv[i] + 1) >> 1) as u8;
                    }
                }
            }
        }
        (2, _) | (_, 2) => {
            // Horizontal half samples (unrounded) for rows -2..h+3, then a
            // vertical pass for the centre position j.
            let mut raw = [0i32; 21 * 16];
            for r in 0..h + 5 {
                let base = row(r as isize - 2);
                let line = &v.s[base - 2..base + w + 3];
                for (d, win6) in raw[r * 16..r * 16 + w].iter_mut().zip(line.windows(6)) {
                    *d = tap(win6);
                }
            }
            let st = v.st;
            let mut hv = [0i32; 16];
            for jy in 0..h {
                let o = &mut out[oo + jy * os..oo + jy * os + w];
                let side = fx != 2;
                if side {
                    vhalf_row(v.s, st, row(jy as isize - 2) + (fx == 3) as usize, w, &mut hv);
                }
                let base = jy * 16;
                let (a0, a1, a2, a3, a4, a5) = (
                    &raw[base..base + w],
                    &raw[base + 16..base + 16 + w],
                    &raw[base + 32..base + 32 + w],
                    &raw[base + 48..base + 48 + w],
                    &raw[base + 64..base + 64 + w],
                    &raw[base + 80..base + 80 + w],
                );
                for i in 0..w {
                    let j1 = tap_i(a0[i], a1[i], a2[i], a3[i], a4[i], a5[i]);
                    let jv = clip_u8((j1 + 512) >> 10) as i32;
                    let val = match (fx, fy) {
                        (2, 2) => jv,
                        (2, 1) => (clip_u8((a2[i] + 16) >> 5) as i32 + jv + 1) >> 1,
                        (2, _) => (clip_u8((a3[i] + 16) >> 5) as i32 + jv + 1) >> 1,
                        _ => (hv[i] + jv + 1) >> 1,
                    };
                    o[i] = val as u8;
                }
            }
        }
        _ => {
            // Diagonal quarter positions: average of a horizontal half
            // sample (row j or j+1) and a vertical one (column i or i+1).
            let st = v.st;
            let dy = (fy == 3) as isize;
            let dx = (fx == 3) as usize;
            let mut hv = [0i32; 16];
            for j in 0..h {
                let rb = row(j as isize + dy);
                let line = &v.s[rb - 2..rb + w + 3];
                vhalf_row(v.s, st, row(j as isize - 2) + dx, w, &mut hv);
                let o = &mut out[oo + j * os..oo + j * os + w];
                for (i, (d, win6)) in o.iter_mut().zip(line.windows(6)).enumerate() {
                    let b = clip_u8((tap(win6) + 16) >> 5) as i32;
                    *d = ((b + hv[i] + 1) >> 1) as u8;
                }
            }
        }
    }
}

/// Chroma prediction (4:2:0, eighth-sample accuracy) of a w x h block.
#[allow(clippy::too_many_arguments)]
#[inline]
pub fn chroma(src: &[u8], stride: usize, pw: usize, ph: usize, x: i32, y: i32, fx: u32, fy: u32, w: usize, h: usize, out: &mut [u8], oo: usize, os: usize) {
    chroma_impl(src, stride, pw, ph, x, y, fx, fy, w, h, out, oo, os)
}

#[allow(clippy::too_many_arguments)]
fn chroma_impl(src: &[u8], stride: usize, pw: usize, ph: usize, x: i32, y: i32, fx: u32, fy: u32, w: usize, h: usize, out: &mut [u8], oo: usize, os: usize) {
    let mut win = [0u8; 9 * 9];
    let inside = x >= 0 && y >= 0 && x as usize + w < pw && y as usize + h < ph;
    let (s, st, o) = if inside {
        (src, stride, y as usize * stride + x as usize)
    } else {
        for r in 0..h + 1 {
            let sy = (y + r as i32).clamp(0, ph as i32 - 1) as usize;
            for c in 0..w + 1 {
                let sx = (x + c as i32).clamp(0, pw as i32 - 1) as usize;
                win[r * 9 + c] = src[sy * stride + sx];
            }
        }
        (&win[..], 9, 0)
    };
    let (fx, fy) = (fx as u32, fy as u32);
    if fx == 0 && fy == 0 {
        for j in 0..h {
            out[oo + j * os..oo + j * os + w].copy_from_slice(&s[o + j * st..o + j * st + w]);
        }
        return;
    }
    let wa = (8 - fx) * (8 - fy);
    let wb = fx * (8 - fy);
    let wc = (8 - fx) * fy;
    let wd = fx * fy;
    for j in 0..h {
        let r0 = &s[o + j * st..o + j * st + w + 1];
        let r1 = &s[o + (j + 1) * st..o + (j + 1) * st + w + 1];
        let d = &mut out[oo + j * os..oo + j * os + w];
        for i in 0..w {
            d[i] = ((wa * r0[i] as u32 + wb * r0[i + 1] as u32 + wc * r1[i] as u32 + wd * r1[i + 1] as u32 + 32) >> 6) as u8;
        }
    }
}
