//! Intra prediction (8.3).

use super::transform::clip_u8;

#[derive(Clone, Copy, Default)]
pub struct Avail {
    pub left: bool,
    pub top: bool,
    pub topright: bool,
    pub topleft: bool,
}

/// Gather edge samples for an NxN block at (x, y): `top[0]` is the top-left
/// sample, `top[1..=2n]` the row above (with the top-right part), and
/// `left[1..=n]` the column to the left.
fn edges(p: &[u8], stride: usize, x: usize, y: usize, n: usize, av: Avail, top: &mut [i32; 17], left: &mut [i32; 17]) {
    if av.top {
        let row = (y - 1) * stride + x;
        for i in 0..n {
            top[1 + i] = p[row + i] as i32;
        }
        if av.topright {
            for i in 0..n {
                top[1 + n + i] = p[row + n + i] as i32;
            }
        } else {
            for i in 0..n {
                top[1 + n + i] = top[n];
            }
        }
    }
    if av.left {
        for i in 0..n {
            left[1 + i] = p[(y + i) * stride + x - 1] as i32;
        }
    }
    if av.topleft {
        top[0] = p[(y - 1) * stride + x - 1] as i32;
        left[0] = top[0];
    }
}

/// 8x8 reference sample filtering (8.3.2.2.1).
fn filter8(top: &mut [i32; 17], left: &mut [i32; 17], av: Avail) {
    let t = *top;
    let l = *left;
    if av.top {
        top[1] = if av.topleft { (t[0] + 2 * t[1] + t[2] + 2) >> 2 } else { (3 * t[1] + t[2] + 2) >> 2 };
        for x in 1..15 {
            top[1 + x] = (t[x] + 2 * t[1 + x] + t[2 + x] + 2) >> 2;
        }
        top[16] = (t[15] + 3 * t[16] + 2) >> 2;
    }
    if av.topleft {
        let v = if av.top && av.left {
            (t[1] + 2 * t[0] + l[1] + 2) >> 2
        } else if av.top {
            (3 * t[0] + t[1] + 2) >> 2
        } else if av.left {
            (3 * t[0] + l[1] + 2) >> 2
        } else {
            t[0]
        };
        top[0] = v;
        left[0] = v;
    }
    if av.left {
        left[1] = if av.topleft { (l[0] + 2 * l[1] + l[2] + 2) >> 2 } else { (3 * l[1] + l[2] + 2) >> 2 };
        for y in 1..7 {
            left[1 + y] = (l[y] + 2 * l[1 + y] + l[2 + y] + 2) >> 2;
        }
        left[8] = (l[7] + 3 * l[8] + 2) >> 2;
    }
}

/// Intra 4x4 (n = 4) or 8x8 (n = 8) prediction written into the picture.
pub fn pred_nxn(p: &mut [u8], stride: usize, x: usize, y: usize, n: usize, mode: u8, av: Avail) {
    let mut top = [0i32; 17];
    let mut left = [0i32; 17];
    edges(p, stride, x, y, n, av, &mut top, &mut left);
    if n == 8 {
        filter8(&mut top, &mut left, av);
    }
    // t(i): sample above at column i (-1 = top-left); l(j): left at row j.
    let t = |i: isize| top[(i + 1) as usize];
    let l = |j: isize| left[(j + 1) as usize];
    let ni = n as isize;
    let mut out = [0i32; 64];
    match mode {
        0 => {
            for yy in 0..n {
                for xx in 0..n {
                    out[yy * n + xx] = top[1 + xx];
                }
            }
        }
        1 => {
            for yy in 0..n {
                for xx in 0..n {
                    out[yy * n + xx] = left[1 + yy];
                }
            }
        }
        2 => {
            let st: i32 = top[1..=n].iter().sum();
            let sl: i32 = left[1..=n].iter().sum();
            let sh = if n == 4 { 2 } else { 3 };
            let dc = match (av.top, av.left) {
                (true, true) => (st + sl + n as i32) >> (sh + 1),
                (false, true) => (sl + (n as i32 >> 1)) >> sh,
                (true, false) => (st + (n as i32 >> 1)) >> sh,
                _ => 128,
            };
            out[..n * n].iter_mut().for_each(|v| *v = dc);
        }
        _ => {
            for yy in 0..ni {
                for xx in 0..ni {
                    let v = match mode {
                        3 => {
                            if xx == ni - 1 && yy == ni - 1 {
                                (t(2 * ni - 2) + 3 * t(2 * ni - 1) + 2) >> 2
                            } else {
                                (t(xx + yy) + 2 * t(xx + yy + 1) + t(xx + yy + 2) + 2) >> 2
                            }
                        }
                        4 => {
                            if xx > yy {
                                (t(xx - yy - 2) + 2 * t(xx - yy - 1) + t(xx - yy) + 2) >> 2
                            } else if xx < yy {
                                (l(yy - xx - 2) + 2 * l(yy - xx - 1) + l(yy - xx) + 2) >> 2
                            } else {
                                (t(0) + 2 * t(-1) + l(0) + 2) >> 2
                            }
                        }
                        5 => {
                            let z = 2 * xx - yy;
                            if z >= 0 && z & 1 == 0 {
                                (t(xx - (yy >> 1) - 1) + t(xx - (yy >> 1)) + 1) >> 1
                            } else if z >= 0 {
                                (t(xx - (yy >> 1) - 2) + 2 * t(xx - (yy >> 1) - 1) + t(xx - (yy >> 1)) + 2) >> 2
                            } else if z == -1 {
                                (l(0) + 2 * l(-1) + t(0) + 2) >> 2
                            } else {
                                (l(yy - 2 * xx - 1) + 2 * l(yy - 2 * xx - 2) + l(yy - 2 * xx - 3) + 2) >> 2
                            }
                        }
                        6 => {
                            let z = 2 * yy - xx;
                            if z >= 0 && z & 1 == 0 {
                                (l(yy - (xx >> 1) - 1) + l(yy - (xx >> 1)) + 1) >> 1
                            } else if z >= 0 {
                                (l(yy - (xx >> 1) - 2) + 2 * l(yy - (xx >> 1) - 1) + l(yy - (xx >> 1)) + 2) >> 2
                            } else if z == -1 {
                                (l(0) + 2 * l(-1) + t(0) + 2) >> 2
                            } else {
                                (t(xx - 2 * yy - 1) + 2 * t(xx - 2 * yy - 2) + t(xx - 2 * yy - 3) + 2) >> 2
                            }
                        }
                        7 => {
                            let b = xx + (yy >> 1);
                            if yy & 1 == 0 {
                                (t(b) + t(b + 1) + 1) >> 1
                            } else {
                                (t(b) + 2 * t(b + 1) + t(b + 2) + 2) >> 2
                            }
                        }
                        _ => {
                            let z = xx + 2 * yy;
                            let b = yy + (xx >> 1);
                            if z > 2 * ni - 3 {
                                l(ni - 1)
                            } else if z == 2 * ni - 3 {
                                (l(ni - 2) + 3 * l(ni - 1) + 2) >> 2
                            } else if z & 1 == 0 {
                                (l(b) + l(b + 1) + 1) >> 1
                            } else {
                                (l(b) + 2 * l(b + 1) + l(b + 2) + 2) >> 2
                            }
                        }
                    };
                    out[(yy * ni + xx) as usize] = v;
                }
            }
        }
    }
    for yy in 0..n {
        let row = (y + yy) * stride + x;
        for xx in 0..n {
            p[row + xx] = out[yy * n + xx] as u8;
        }
    }
}

/// Intra 16x16 prediction.
pub fn pred16(p: &mut [u8], stride: usize, x: usize, y: usize, mode: u8, av: Avail) {
    let mut top = [0i32; 16];
    let mut left = [0i32; 16];
    if av.top {
        for i in 0..16 {
            top[i] = p[(y - 1) * stride + x + i] as i32;
        }
    }
    if av.left {
        for i in 0..16 {
            left[i] = p[(y + i) * stride + x - 1] as i32;
        }
    }
    match mode {
        0 => {
            for yy in 0..16 {
                for xx in 0..16 {
                    p[(y + yy) * stride + x + xx] = top[xx] as u8;
                }
            }
        }
        1 => {
            for yy in 0..16 {
                for xx in 0..16 {
                    p[(y + yy) * stride + x + xx] = left[yy] as u8;
                }
            }
        }
        2 => {
            let st: i32 = top.iter().sum();
            let sl: i32 = left.iter().sum();
            let dc = match (av.top, av.left) {
                (true, true) => (st + sl + 16) >> 5,
                (false, true) => (sl + 8) >> 4,
                (true, false) => (st + 8) >> 4,
                _ => 128,
            } as u8;
            for yy in 0..16 {
                for xx in 0..16 {
                    p[(y + yy) * stride + x + xx] = dc;
                }
            }
        }
        _ => {
            let tl = p[(y - 1) * stride + x - 1] as i32;
            let tp = |i: isize| if i < 0 { tl } else { top[i as usize] };
            let lp = |i: isize| if i < 0 { tl } else { left[i as usize] };
            let mut h = 0;
            let mut v = 0;
            for k in 0..8isize {
                h += (k + 1) as i32 * (tp(8 + k) - tp(6 - k));
                v += (k + 1) as i32 * (lp(8 + k) - lp(6 - k));
            }
            let a = 16 * (left[15] + top[15]);
            let b = (5 * h + 32) >> 6;
            let c = (5 * v + 32) >> 6;
            for yy in 0..16i32 {
                for xx in 0..16i32 {
                    p[(y + yy as usize) * stride + x + xx as usize] = clip_u8((a + b * (xx - 7) + c * (yy - 7) + 16) >> 5);
                }
            }
        }
    }
}

/// Chroma intra prediction for one 8x8 (4:2:0) plane.
pub fn pred_chroma(p: &mut [u8], stride: usize, x: usize, y: usize, mode: u8, av: Avail) {
    let mut top = [0i32; 8];
    let mut left = [0i32; 8];
    if av.top {
        for i in 0..8 {
            top[i] = p[(y - 1) * stride + x + i] as i32;
        }
    }
    if av.left {
        for i in 0..8 {
            left[i] = p[(y + i) * stride + x - 1] as i32;
        }
    }
    match mode {
        0 => {
            for by in 0..2 {
                for bx in 0..2 {
                    let st: i32 = top[bx * 4..bx * 4 + 4].iter().sum();
                    let sl: i32 = left[by * 4..by * 4 + 4].iter().sum();
                    let dc = if bx == by {
                        match (av.top, av.left) {
                            (true, true) => (st + sl + 4) >> 3,
                            (true, false) => (st + 2) >> 2,
                            (false, true) => (sl + 2) >> 2,
                            _ => 128,
                        }
                    } else if bx == 1 {
                        if av.top {
                            (st + 2) >> 2
                        } else if av.left {
                            (sl + 2) >> 2
                        } else {
                            128
                        }
                    } else if av.left {
                        (sl + 2) >> 2
                    } else if av.top {
                        (st + 2) >> 2
                    } else {
                        128
                    };
                    for yy in 0..4 {
                        for xx in 0..4 {
                            p[(y + by * 4 + yy) * stride + x + bx * 4 + xx] = dc as u8;
                        }
                    }
                }
            }
        }
        1 => {
            for yy in 0..8 {
                for xx in 0..8 {
                    p[(y + yy) * stride + x + xx] = left[yy] as u8;
                }
            }
        }
        2 => {
            for yy in 0..8 {
                for xx in 0..8 {
                    p[(y + yy) * stride + x + xx] = top[xx] as u8;
                }
            }
        }
        _ => {
            let tl = p[(y - 1) * stride + x - 1] as i32;
            let tp = |i: isize| if i < 0 { tl } else { top[i as usize] };
            let lp = |i: isize| if i < 0 { tl } else { left[i as usize] };
            let mut h = 0;
            let mut v = 0;
            for k in 0..4isize {
                h += (k + 1) as i32 * (tp(4 + k) - tp(2 - k));
                v += (k + 1) as i32 * (lp(4 + k) - lp(2 - k));
            }
            let a = 16 * (left[7] + top[7]);
            let b = (34 * h + 32) >> 6;
            let c = (34 * v + 32) >> 6;
            for yy in 0..8i32 {
                for xx in 0..8i32 {
                    p[(y + yy as usize) * stride + x + xx as usize] = clip_u8((a + b * (xx - 3) + c * (yy - 3) + 16) >> 5);
                }
            }
        }
    }
}
