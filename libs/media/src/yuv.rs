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
