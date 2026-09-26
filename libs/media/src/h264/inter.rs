//! Fractional-sample interpolation for motion compensation (8.4.2.2).
//! The arithmetic is in csrc/h264dsp.c (vectorised C); this side finds
//! the samples, padding blocks that reach outside the picture.

const WS: usize = 24; // window stride

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
    let _ = out[oo + (h - 1) * os + w - 1];
    // SAFETY: `v` has samples from 2 before to 3 after the block in both
    // directions, and `out` holds h rows of w samples at stride `os`.
    unsafe {
        mayos_h264_luma(v.s.as_ptr().add(v.o), v.st as i32, fx as i32, fy as i32, w as i32, h as i32, out[oo..].as_mut_ptr(), os as i32);
    }
}

unsafe extern "C" {
    fn mayos_h264_luma(s: *const u8, st: i32, fx: i32, fy: i32, w: i32, h: i32, out: *mut u8, os: i32);
    fn mayos_h264_chroma(s: *const u8, st: i32, fx: i32, fy: i32, w: i32, h: i32, out: *mut u8, os: i32);
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
    let _ = out[oo + (h - 1) * os + w - 1];
    // SAFETY: `s` holds (w + 1) x (h + 1) samples from `o`; `out` is
    // checked above.
    unsafe { mayos_h264_chroma(s.as_ptr().add(o), st as i32, fx as i32, fy as i32, w as i32, h as i32, out[oo..].as_mut_ptr(), os as i32) }
}
