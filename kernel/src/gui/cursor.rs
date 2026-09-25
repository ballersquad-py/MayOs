//! The mouse pointer image, rasterised once at boot with 4x4 supersampling.

use alloc::vec;
use alloc::vec::Vec;

use super::display::CURSOR_DIM;

const SS: i32 = 4;

/// Classic arrow outline, in pixels (hotspot at the origin).
const ARROW: &[(i32, i32)] = &[(1, 1), (1, 18), (5, 14), (8, 21), (11, 20), (8, 13), (14, 13)];

fn inside(poly: &[(i32, i32)], x: i32, y: i32) -> bool {
    // Point in polygon (even-odd), coordinates in supersampled units.
    let mut c = false;
    let n = poly.len();
    let mut j = n - 1;
    for i in 0..n {
        let (xi, yi) = (poly[i].0 * SS, poly[i].1 * SS);
        let (xj, yj) = (poly[j].0 * SS, poly[j].1 * SS);
        if (yi > y) != (yj > y) && x < (xj - xi) * (y - yi) / (yj - yi) + xi {
            c = !c;
        }
        j = i;
    }
    c
}

/// A 64x64 ARGB arrow: white fill with a dark antialiased outline and a
/// soft shadow.
pub fn arrow() -> (Vec<u32>, (i32, i32)) {
    let n = (CURSOR_DIM * SS) as usize;
    let mut mask = vec![false; n * n];
    for y in 0..n {
        for x in 0..n {
            mask[y * n + x] = inside(ARROW, x as i32, y as i32);
        }
    }
    // Dilate for the outline (1.25 px).
    let r = 5i32;
    let mut outline = vec![false; n * n];
    for y in 0..n as i32 {
        for x in 0..n as i32 {
            if !mask[(y * n as i32 + x) as usize] {
                continue;
            }
            for dy in -r..=r {
                for dx in -r..=r {
                    if dx * dx + dy * dy > r * r {
                        continue;
                    }
                    let (xx, yy) = (x + dx, y + dy);
                    if xx >= 0 && yy >= 0 && xx < n as i32 && yy < n as i32 {
                        outline[(yy * n as i32 + xx) as usize] = true;
                    }
                }
            }
        }
    }
    let d = CURSOR_DIM as usize;
    let mut out = vec![0u32; d * d];
    for py in 0..d {
        for px in 0..d {
            let (mut fill, mut edge, mut shadow) = (0u32, 0u32, 0u32);
            for sy in 0..SS as usize {
                for sx in 0..SS as usize {
                    let i = (py * SS as usize + sy) * n + px * SS as usize + sx;
                    if mask[i] {
                        fill += 1;
                    } else if outline[i] {
                        edge += 1;
                    }
                    // Shadow: outline shifted by (1, 2) pixels.
                    let (ox, oy) = ((px * SS as usize + sx) as i32 - SS, (py * SS as usize + sy) as i32 - 2 * SS);
                    if ox >= 0 && oy >= 0 && outline[oy as usize * n + ox as usize] {
                        shadow += 1;
                    }
                }
            }
            let total = (SS * SS) as u32;
            let fa = fill * 255 / total;
            let ea = edge * 255 / total;
            let sa = shadow * 70 / total;
            // Composite: shadow, then outline (dark), then fill (white).
            let mut a = sa;
            let mut c = 0u32; // colour channel value (grey)
            let over = |a_dst: u32, c_dst: u32, a_src: u32, c_src: u32| -> (u32, u32) {
                let a_out = a_src + a_dst * (255 - a_src) / 255;
                if a_out == 0 {
                    return (0, 0);
                }
                let c_out = (c_src * a_src + c_dst * a_dst * (255 - a_src) / 255) / a_out;
                (a_out, c_out)
            };
            (a, c) = over(a, c, ea, 0x20);
            (a, c) = over(a, c, fa, 0xff);
            out[py * d + px] = (a << 24) | (c << 16) | (c << 8) | c;
        }
    }
    (out, (1, 1))
}
