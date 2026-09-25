//! Inverse transforms and dequantisation (8.5).

#[inline]
pub fn clip_u8(v: i32) -> u8 {
    if v < 0 { 0 } else if v > 255 { 255 } else { v as u8 }
}

/// 4x4 inverse transform of `c` (raster, already scaled) added to the
/// prediction in `dst` (stride `stride`).
pub fn idct4_add(c: &mut [i32; 16], dst: &mut [u8], off: usize, stride: usize) {
    for i in 0..4 {
        let r = &mut c[i * 4..i * 4 + 4];
        let e = r[0] + r[2];
        let f = r[0] - r[2];
        let g = (r[1] >> 1) - r[3];
        let h = r[1] + (r[3] >> 1);
        r[0] = e + h;
        r[1] = f + g;
        r[2] = f - g;
        r[3] = e - h;
    }
    for j in 0..4 {
        let (a, b, cc, d) = (c[j], c[4 + j], c[8 + j], c[12 + j]);
        let e = a + cc;
        let f = a - cc;
        let g = (b >> 1) - d;
        let h = b + (d >> 1);
        let col = [e + h, f + g, f - g, e - h];
        for i in 0..4 {
            let p = off + i * stride + j;
            dst[p] = clip_u8(dst[p] as i32 + ((col[i] + 32) >> 6));
        }
    }
}

/// DC-only shortcut.
pub fn idct4_dc_add(dc: i32, dst: &mut [u8], off: usize, stride: usize, size: usize) {
    let d = (dc + 32) >> 6;
    for i in 0..size {
        for j in 0..size {
            let p = off + i * stride + j;
            dst[p] = clip_u8(dst[p] as i32 + d);
        }
    }
}

pub fn idct8_add(c: &mut [i32; 64], dst: &mut [u8], off: usize, stride: usize) {
    fn pass(v: [i32; 8]) -> [i32; 8] {
        let a0 = v[0] + v[4];
        let a4 = v[0] - v[4];
        let a2 = (v[2] >> 1) - v[6];
        let a6 = v[2] + (v[6] >> 1);
        let b0 = a0 + a6;
        let b2 = a4 + a2;
        let b4 = a4 - a2;
        let b6 = a0 - a6;
        let a1 = -v[3] + v[5] - v[7] - (v[7] >> 1);
        let a3 = v[1] + v[7] - v[3] - (v[3] >> 1);
        let a5 = -v[1] + v[7] + v[5] + (v[5] >> 1);
        let a7 = v[3] + v[5] + v[1] + (v[1] >> 1);
        let b1 = a1 + (a7 >> 2);
        let b7 = a7 - (a1 >> 2);
        let b3 = a3 + (a5 >> 2);
        let b5 = (a3 >> 2) - a5;
        [b0 + b7, b2 + b5, b4 + b3, b6 + b1, b6 - b1, b4 - b3, b2 - b5, b0 - b7]
    }
    for i in 0..8 {
        let mut r = [0i32; 8];
        r.copy_from_slice(&c[i * 8..i * 8 + 8]);
        let o = pass(r);
        c[i * 8..i * 8 + 8].copy_from_slice(&o);
    }
    for j in 0..8 {
        let mut col = [0i32; 8];
        for i in 0..8 {
            col[i] = c[i * 8 + j];
        }
        let o = pass(col);
        for i in 0..8 {
            let p = off + i * stride + j;
            dst[p] = clip_u8(dst[p] as i32 + ((o[i] + 32) >> 6));
        }
    }
}

/// Inverse Hadamard of the 16 Intra16x16 DC coefficients (raster 4x4),
/// followed by scaling with `scale` = LevelScale4x4(qp % 6, 0, 0).
pub fn luma_dc_dequant(c: &mut [i32; 16], qp: i32, scale: i32) {
    let mut t = [0i32; 16];
    for i in 0..4 {
        let (a, b, cc, d) = (c[i * 4], c[i * 4 + 1], c[i * 4 + 2], c[i * 4 + 3]);
        t[i * 4] = a + b + cc + d;
        t[i * 4 + 1] = a + b - cc - d;
        t[i * 4 + 2] = a - b - cc + d;
        t[i * 4 + 3] = a - b + cc - d;
    }
    for j in 0..4 {
        let (a, b, cc, d) = (t[j], t[4 + j], t[8 + j], t[12 + j]);
        let f = [a + b + cc + d, a + b - cc - d, a - b - cc + d, a - b + cc - d];
        for i in 0..4 {
            let v = f[i];
            c[i * 4 + j] = if qp >= 36 {
                (v * scale) << (qp / 6 - 6)
            } else {
                (v * scale + (1 << (5 - qp / 6))) >> (6 - qp / 6)
            };
        }
    }
}

/// 2x2 chroma DC transform and scaling.
pub fn chroma_dc_dequant(c: &mut [i32; 4], qp: i32, scale: i32) {
    let (a, b, cc, d) = (c[0], c[1], c[2], c[3]);
    let f = [a + b + cc + d, a - b + cc - d, a + b - cc - d, a - b - cc + d];
    for i in 0..4 {
        c[i] = ((f[i] * scale) << (qp / 6)) >> 5;
    }
}

/// Dequantise a 4x4 block in place (raster coefficients, raster `scale`
/// = LevelScale4x4 for qp % 6). `skip_dc` leaves coefficient 0 alone.
#[inline]
pub fn dequant4(c: &mut [i32; 16], qp: i32, scale: &[i32; 16], skip_dc: bool) {
    let start = skip_dc as usize;
    if qp >= 24 {
        let sh = qp / 6 - 4;
        for i in start..16 {
            if c[i] != 0 {
                c[i] = (c[i] * scale[i]) << sh;
            }
        }
    } else {
        let sh = 4 - qp / 6;
        let add = 1 << (sh - 1);
        for i in start..16 {
            if c[i] != 0 {
                c[i] = (c[i] * scale[i] + add) >> sh;
            }
        }
    }
}

#[inline]
pub fn dequant8(c: &mut [i32; 64], qp: i32, scale: &[i32; 64]) {
    if qp >= 36 {
        let sh = qp / 6 - 6;
        for i in 0..64 {
            if c[i] != 0 {
                c[i] = (c[i] * scale[i]) << sh;
            }
        }
    } else {
        let sh = 6 - qp / 6;
        let add = 1 << (sh - 1);
        for i in 0..64 {
            if c[i] != 0 {
                c[i] = (c[i] * scale[i] + add) >> sh;
            }
        }
    }
}

const NORM4: [[i32; 3]; 6] = [[10, 16, 13], [11, 18, 14], [13, 20, 16], [14, 23, 18], [16, 25, 20], [18, 29, 23]];
const NORM8: [[i32; 6]; 6] = [
    [20, 18, 32, 19, 25, 24],
    [22, 19, 35, 21, 28, 26],
    [26, 23, 42, 24, 33, 31],
    [28, 25, 45, 26, 35, 33],
    [32, 28, 51, 30, 40, 38],
    [36, 32, 58, 34, 46, 43],
];

/// LevelScale4x4 for one scaling list: [qp % 6][raster position].
pub fn level_scale4(list: &[u8; 16]) -> [[i32; 16]; 6] {
    let mut o = [[0i32; 16]; 6];
    for m in 0..6 {
        for p in 0..16 {
            let (i, j) = (p / 4, p % 4);
            let n = if i % 2 == 0 && j % 2 == 0 {
                NORM4[m][0]
            } else if i % 2 == 1 && j % 2 == 1 {
                NORM4[m][1]
            } else {
                NORM4[m][2]
            };
            o[m][p] = list[p] as i32 * n;
        }
    }
    o
}

pub fn level_scale8(list: &[u8; 64]) -> [[i32; 64]; 6] {
    let mut o = [[0i32; 64]; 6];
    for m in 0..6 {
        for p in 0..64 {
            let (i, j) = (p / 8, p % 8);
            let n = if i % 4 == 0 && j % 4 == 0 {
                NORM8[m][0]
            } else if i % 2 == 1 && j % 2 == 1 {
                NORM8[m][1]
            } else if i % 4 == 2 && j % 4 == 2 {
                NORM8[m][2]
            } else if (i % 4 == 0 && j % 2 == 1) || (i % 2 == 1 && j % 4 == 0) {
                NORM8[m][3]
            } else if (i % 4 == 0 && j % 4 == 2) || (i % 4 == 2 && j % 4 == 0) {
                NORM8[m][4]
            } else {
                NORM8[m][5]
            };
            o[m][p] = list[p] as i32 * n;
        }
    }
    o
}
