//! YUV 4:2:0 to RGB conversion (integer, BT.601 / BT.709, limited or full range).

use alloc::vec;
use alloc::vec::Vec;

#[allow(clippy::too_many_arguments)]
pub fn to_argb(
    y: &[u8],
    u: &[u8],
    v: &[u8],
    ys: usize,
    cs: usize,
    cx: usize,
    cy: usize,
    w: usize,
    h: usize,
    bt709: bool,
    full: bool,
    out: &mut [u32],
) {
    to_argb_impl(y, u, v, ys, cs, cx, cy, w, h, bt709, full, out)
}

#[allow(clippy::too_many_arguments)]
fn to_argb_impl(
    y: &[u8],
    u: &[u8],
    v: &[u8],
    ys: usize,
    cs: usize,
    cx: usize,
    cy: usize,
    w: usize,
    h: usize,
    bt709: bool,
    full: bool,
    out: &mut [u32],
) {
    // Coefficients in 1/1024 units.
    let (cr_r, cb_g, cr_g, cb_b) = match (bt709, full) {
        (false, false) => (1634, 401, 832, 2066),
        (true, false) => (1836, 218, 546, 2163),
        (false, true) => (1436, 352, 731, 1815),
        (true, true) => (1613, 192, 479, 1900),
    };
    let (ymul, yoff) = if full { (1024, 0) } else { (1192, 16) };
    for j in 0..h {
        let yr = (cy + j) * ys + cx;
        let cr_ = ((cy + j) / 2) * cs;
        let o = &mut out[j * w..j * w + w];
        for i in 0..w {
            let yy = (y[yr + i] as i32 - yoff) * ymul;
            let ci = cr_ + (cx + i) / 2;
            let uu = u[ci] as i32 - 128;
            let vv = v[ci] as i32 - 128;
            let r = (yy + cr_r * vv + 512) >> 10;
            let g = (yy - cb_g * uu - cr_g * vv + 512) >> 10;
            let b = (yy + cb_b * uu + 512) >> 10;
            o[i] = 0xff00_0000 | ((r.clamp(0, 255) as u32) << 16) | ((g.clamp(0, 255) as u32) << 8) | b.clamp(0, 255) as u32;
        }
    }
}

/// Bilinear scale + convert a cropped YUV 4:2:0 picture into `dst`.
#[allow(clippy::too_many_arguments)]
pub fn scale_to_argb(
    y: &[u8],
    u: &[u8],
    v: &[u8],
    ys: usize,
    cs: usize,
    cx: usize,
    cy: usize,
    w: usize,
    h: usize,
    bt709: bool,
    full: bool,
    dst: &mut [u32],
    dw: usize,
    dh: usize,
    ds: usize,
) {
    scale_to_argb_impl(y, u, v, ys, cs, cx, cy, w, h, bt709, full, dst, dw, dh, ds)
}

#[allow(clippy::too_many_arguments)]
fn scale_to_argb_impl(
    y: &[u8],
    u: &[u8],
    v: &[u8],
    ys: usize,
    cs: usize,
    cx: usize,
    cy: usize,
    w: usize,
    h: usize,
    bt709: bool,
    full: bool,
    dst: &mut [u32],
    dw: usize,
    dh: usize,
    ds: usize,
) {
    if dw == 0 || dh == 0 || w == 0 || h == 0 {
        return;
    }
    let (cr_r, cb_g, cr_g, cb_b) = match (bt709, full) {
        (false, false) => (1634, 401, 832, 2066),
        (true, false) => (1836, 218, 546, 2163),
        (false, true) => (1436, 352, 731, 1815),
        (true, true) => (1613, 192, 479, 1900),
    };
    let (ymul, yoff) = if full { (1024, 0) } else { (1192, 16) };
    // Source positions (16.16) of each destination column and row, as
    // tables for the C loop (csrc/h264dsp.c).
    let step_x = ((w as u64) << 16) / dw as u64;
    let step_y = ((h as u64) << 16) / dh as u64;
    let (mut x0s, mut x1s, mut xfs, mut xcs) = (Vec::with_capacity(dw), Vec::with_capacity(dw), Vec::with_capacity(dw), Vec::with_capacity(dw));
    let mut acc = step_x / 2;
    for _ in 0..dw {
        let sx = if acc > (1 << 15) { (acc - (1 << 15)) as usize } else { 0 };
        acc += step_x;
        let x0 = (sx >> 16).min(w - 1);
        x0s.push((cx + x0) as i32);
        x1s.push((cx + (x0 + 1).min(w - 1)) as i32);
        xfs.push(((sx >> 8) & 0xff) as i32);
        xcs.push(((sx / 2 >> 16) + cx / 2).min((cx + w) / 2 - 1) as i32);
    }
    let (mut ry0, mut ry1, mut rfy, mut rc) = (Vec::with_capacity(dh), Vec::with_capacity(dh), Vec::with_capacity(dh), Vec::with_capacity(dh));
    let mut accy = step_y / 2;
    for _ in 0..dh {
        let sy = if accy > (1 << 15) { (accy - (1 << 15)) as usize } else { 0 };
        accy += step_y;
        let y0 = (sy >> 16).min(h - 1);
        ry0.push(((cy + y0) * ys) as i32);
        ry1.push(((cy + (y0 + 1).min(h - 1)) * ys) as i32);
        rfy.push(((sy >> 8) & 0xff) as i32);
        rc.push((((cy / 2) + (sy / 2 >> 16).min(h / 2 - 1).min((cy + h) / 2 - 1)) * cs) as i32);
    }
    // Every index stays inside the planes and `dst`.
    let (xb, xe) = (cx, cx + w);
    assert!(ry1.iter().chain(&ry0).all(|&r| r as usize + xe <= y.len()));
    assert!(rc.iter().all(|&r| r as usize + (cx + w).div_ceil(2) <= u.len().min(v.len())));
    assert!(xe <= ys && (dh - 1) * ds + dw <= dst.len());
    let coef = [cr_r, cb_g, cr_g, cb_b, ymul, yoff];
    let mut tmp = vec![0u16; xe];
    let (mut yb, mut ub, mut vb) = (vec![0i16; dw], vec![0i16; dw], vec![0i16; dw]);
    // SAFETY: the tables were checked against the buffer sizes above.
    unsafe {
        mayos_yuv_scale(
            y.as_ptr(), u.as_ptr(), v.as_ptr(),
            ry0.as_ptr(), ry1.as_ptr(), rfy.as_ptr(), rc.as_ptr(), dh as i32,
            x0s.as_ptr(), x1s.as_ptr(), xfs.as_ptr(), xcs.as_ptr(), dw as i32,
            xb as i32, xe as i32, coef.as_ptr(), dst.as_mut_ptr(), ds as i32,
            tmp.as_mut_ptr(), yb.as_mut_ptr(), ub.as_mut_ptr(), vb.as_mut_ptr(),
        );
    }
}

unsafe extern "C" {
    fn mayos_yuv_scale(
        y: *const u8, u: *const u8, v: *const u8,
        ry0: *const i32, ry1: *const i32, rfy: *const i32, rc: *const i32, dh: i32,
        x0: *const i32, x1: *const i32, xf: *const i32, xc: *const i32, dw: i32,
        xb: i32, xe: i32, coef: *const i32, dst: *mut u32, ds: i32,
        tmp: *mut u16, yb: *mut i16, ub: *mut i16, vb: *mut i16,
    );
}

/// Bilinear scale of 0xAARRGGBB pixels.
#[allow(clippy::too_many_arguments)]
pub fn scale_argb(src: &[u32], w: usize, h: usize, dst: &mut [u32], dw: usize, dh: usize, ds: usize) {
    scale_argb_impl(src, w, h, dst, dw, dh, ds)
}

#[allow(clippy::too_many_arguments)]
fn scale_argb_impl(src: &[u32], w: usize, h: usize, dst: &mut [u32], dw: usize, dh: usize, ds: usize) {
    if dw == 0 || dh == 0 || w == 0 || h == 0 {
        return;
    }
    let step_x = ((w as u64) << 16) / dw as u64;
    let step_y = ((h as u64) << 16) / dh as u64;
    let mut cols = alloc::vec::Vec::with_capacity(dw);
    let mut acc = step_x / 2;
    for _ in 0..dw {
        let sx = if acc > (1 << 15) { (acc - (1 << 15)) as usize } else { 0 };
        acc += step_x;
        let x0 = (sx >> 16).min(w - 1);
        cols.push((x0, (x0 + 1).min(w - 1), ((sx >> 8) & 0xff) as u32));
    }
    let mut accy = step_y / 2;
    for j in 0..dh {
        let sy = if accy > (1 << 15) { (accy - (1 << 15)) as usize } else { 0 };
        accy += step_y;
        let y0 = (sy >> 16).min(h - 1);
        let y1 = (y0 + 1).min(h - 1);
        let fy = ((sy >> 8) & 0xff) as u32;
        for i in 0..dw {
            let (x0, x1, fx) = cols[i];
            let (p00, p01, p10, p11) = (src[y0 * w + x0], src[y0 * w + x1], src[y1 * w + x0], src[y1 * w + x1]);
            let mut out = 0xff00_0000u32;
            for sh in [0u32, 8, 16] {
                let c = |p: u32| (p >> sh) & 0xff;
                let a = c(p00) * (256 - fx) + c(p01) * fx;
                let b = c(p10) * (256 - fx) + c(p11) * fx;
                out |= ((a * (256 - fy) + b * fy + (1 << 15)) >> 16) << sh;
            }
            dst[j * ds + i] = out;
        }
    }
}
