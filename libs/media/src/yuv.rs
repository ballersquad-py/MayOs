//! YUV 4:2:0 to RGB conversion (integer, BT.601 / BT.709, limited or full range).

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
    // Source positions (16.16) of each destination column, luma and chroma.
    let step_x = ((w as u64) << 16) / dw as u64;
    let step_y = ((h as u64) << 16) / dh as u64;
    let mut lx = alloc::vec::Vec::with_capacity(dw);
    let mut cxs = alloc::vec::Vec::with_capacity(dw);
    // Sample centres: (i + 0.5) * step - 0.5, stepped incrementally.
    let mut acc = step_x / 2;
    for _ in 0..dw {
        let sx = if acc > (1 << 15) { (acc - (1 << 15)) as usize } else { 0 };
        acc += step_x;
        let x0 = (sx >> 16).min(w - 1);
        let x1 = (x0 + 1).min(w - 1);
        lx.push((cx + x0, cx + x1, ((sx >> 8) & 0xff) as i32));
        let csx = sx / 2;
        let c0 = ((csx >> 16) + cx / 2).min((cx + w) / 2 - 1);
        cxs.push(c0);
    }
    let mut accy = step_y / 2;
    for j in 0..dh {
        let sy = if accy > (1 << 15) { (accy - (1 << 15)) as usize } else { 0 };
        accy += step_y;
        let y0 = (sy >> 16).min(h - 1);
        let y1 = (y0 + 1).min(h - 1);
        let fy = ((sy >> 8) & 0xff) as i32;
        let r0 = &y[(cy + y0) * ys..];
        let r1 = &y[(cy + y1) * ys..];
        let crow = ((cy / 2) + (sy / 2 >> 16).min(h / 2 - 1).min((cy + h) / 2 - 1)) * cs;
        let o = &mut dst[j * ds..j * ds + dw];
        for i in 0..dw {
            let (x0, x1, fx) = lx[i];
            let a = r0[x0] as i32 * (256 - fx) + r0[x1] as i32 * fx;
            let b = r1[x0] as i32 * (256 - fx) + r1[x1] as i32 * fx;
            let yv = (a * (256 - fy) + b * fy + (1 << 15)) >> 16;
            let ci = crow + cxs[i];
            let uu = u[ci] as i32 - 128;
            let vv = v[ci] as i32 - 128;
            let yy = (yv - yoff) * ymul;
            let r = (yy + cr_r * vv + 512) >> 10;
            let g = (yy - cb_g * uu - cr_g * vv + 512) >> 10;
            let bb = (yy + cb_b * uu + 512) >> 10;
            o[i] = 0xff00_0000 | ((r.clamp(0, 255) as u32) << 16) | ((g.clamp(0, 255) as u32) << 8) | bb.clamp(0, 255) as u32;
        }
    }
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
