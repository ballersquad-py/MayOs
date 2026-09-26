//! MayOS 2D graphics: surfaces, antialiased shapes, soft shadows and text.
//!
//! Everything uses integer / fixed-point arithmetic. The kernel is built
//! without SSE, so floating point would be emulated in software and slow.
//!
//! Colors are `0xAARRGGBB`. Surfaces are opaque; alpha in a color means
//! "blend this color over the surface".

#![cfg_attr(not(test), no_std)]

extern crate alloc;

pub mod font;
pub mod icons;

use alloc::vec;
use alloc::vec::Vec;

pub use font::Font;

pub type Color = u32;

pub const fn rgb(r: u8, g: u8, b: u8) -> Color {
    0xff00_0000 | (r as u32) << 16 | (g as u32) << 8 | b as u32
}

pub const fn rgba(r: u8, g: u8, b: u8, a: u8) -> Color {
    (a as u32) << 24 | (r as u32) << 16 | (g as u32) << 8 | b as u32
}

pub const fn with_alpha(c: Color, a: u8) -> Color {
    (c & 0x00ff_ffff) | (a as u32) << 24
}

#[inline]
pub fn alpha(c: Color) -> u32 {
    c >> 24
}

/// Blend `src` over `dst` with coverage `a` (0..=255). Result is opaque.
#[inline]
pub fn blend(dst: u32, src: u32, a: u32) -> u32 {
    if a == 0 {
        return dst;
    }
    if a >= 255 {
        return src | 0xff00_0000;
    }
    let inv = 255 - a;
    let mut rb = (src & 0x00ff_00ff) * a + (dst & 0x00ff_00ff) * inv;
    rb = ((rb + 0x0080_0080 + ((rb >> 8) & 0x00ff_00ff)) >> 8) & 0x00ff_00ff;
    let mut g = (src & 0x0000_ff00) * a + (dst & 0x0000_ff00) * inv;
    g = ((g + 0x0000_8000 + ((g >> 8) & 0x0000_ff00)) >> 8) & 0x0000_ff00;
    0xff00_0000 | rb | g
}

/// Multiply a colour's alpha by `opacity` (0..=255).
pub const fn fade(c: Color, opacity: u32) -> Color {
    let a = (c >> 24) * opacity / 255;
    (c & 0x00ff_ffff) | a << 24
}

/// Linear interpolation between two colors, `t` in 0..=255.
pub fn mix(a: Color, b: Color, t: u32) -> Color {
    let t = t.min(255);
    let ch = |s: u32| {
        let x = (a >> s) & 0xff;
        let y = (b >> s) & 0xff;
        ((x * (255 - t) + y * t) / 255) << s
    };
    ch(24) | ch(16) | ch(8) | ch(0)
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Rect {
    pub const fn new(x: i32, y: i32, w: i32, h: i32) -> Rect {
        Rect { x, y, w, h }
    }
    pub fn right(&self) -> i32 {
        self.x + self.w
    }
    pub fn bottom(&self) -> i32 {
        self.y + self.h
    }
    pub fn is_empty(&self) -> bool {
        self.w <= 0 || self.h <= 0
    }
    pub fn contains(&self, x: i32, y: i32) -> bool {
        x >= self.x && y >= self.y && x < self.right() && y < self.bottom()
    }
    pub fn intersect(&self, o: &Rect) -> Rect {
        let x = self.x.max(o.x);
        let y = self.y.max(o.y);
        let r = self.right().min(o.right());
        let b = self.bottom().min(o.bottom());
        Rect { x, y, w: (r - x).max(0), h: (b - y).max(0) }
    }
    pub fn union(&self, o: &Rect) -> Rect {
        if self.is_empty() {
            return *o;
        }
        if o.is_empty() {
            return *self;
        }
        let x = self.x.min(o.x);
        let y = self.y.min(o.y);
        let r = self.right().max(o.right());
        let b = self.bottom().max(o.bottom());
        Rect { x, y, w: r - x, h: b - y }
    }
    pub fn intersects(&self, o: &Rect) -> bool {
        !self.intersect(o).is_empty()
    }
    pub fn inset(&self, d: i32) -> Rect {
        Rect { x: self.x + d, y: self.y + d, w: self.w - 2 * d, h: self.h - 2 * d }
    }
    pub fn offset(&self, dx: i32, dy: i32) -> Rect {
        Rect { x: self.x + dx, y: self.y + dy, w: self.w, h: self.h }
    }
}

/// An owned, opaque pixel buffer.
#[derive(Clone)]
pub struct Surface {
    pub w: i32,
    pub h: i32,
    pub data: Vec<u32>,
}

impl Surface {
    pub fn new(w: i32, h: i32, fill: Color) -> Surface {
        let w = w.max(1);
        let h = h.max(1);
        Surface { w, h, data: vec![fill | 0xff00_0000; (w * h) as usize] }
    }

    pub fn canvas(&mut self) -> Canvas<'_> {
        let (w, h) = (self.w, self.h);
        Canvas::new(&mut self.data, w, h, w as usize)
    }

    pub fn pixel(&self, x: i32, y: i32) -> u32 {
        self.data[(y * self.w + x) as usize]
    }
}

/// Integer square root (floor).
pub fn isqrt(n: u64) -> u64 {
    if n < 2 {
        return n;
    }
    let mut x = 1u64 << ((64 - n.leading_zeros()).div_ceil(2));
    loop {
        let y = (x + n / x) / 2;
        if y >= x {
            return x;
        }
        x = y;
    }
}

/// Coverage (0..=256) of pixel `(px, py)` by a rounded rectangle with
/// integer bounds and radius `r`. Uses the distance from the pixel centre
/// to the corner circle, giving a one-pixel antialiased edge.
#[inline]
fn rrect_coverage(px: i32, py: i32, x0: i32, y0: i32, x1: i32, y1: i32, r: i32) -> u32 {
    let cx = if px < x0 + r {
        x0 + r
    } else if px >= x1 - r {
        x1 - r
    } else {
        return 256;
    };
    let cy = if py < y0 + r {
        y0 + r
    } else if py >= y1 - r {
        y1 - r
    } else {
        return 256;
    };
    let dx = (px * 256 + 128 - cx * 256) as i64;
    let dy = (py * 256 + 128 - cy * 256) as i64;
    let d = isqrt((dx * dx + dy * dy) as u64) as i64;
    (r as i64 * 256 + 128 - d).clamp(0, 256) as u32
}

/// Signed distance (in 1/16 px, positive outside) from a pixel centre to a
/// rounded rectangle.
fn rrect_distance16(px: i32, py: i32, r: &Rect, radius: i32) -> i32 {
    // Work in 1/16 pixel units, relative to the rectangle centre, sampling
    // at pixel centres.
    let cx2 = (r.x * 2 + r.w) * 8;
    let cy2 = (r.y * 2 + r.h) * 8;
    let px2 = (px * 2 + 1) * 8;
    let py2 = (py * 2 + 1) * 8;
    let hx = r.w * 8 - radius * 16;
    let hy = r.h * 8 - radius * 16;
    let qx = (px2 - cx2).abs() - hx;
    let qy = (py2 - cy2).abs() - hy;
    let ox = qx.max(0) as i64;
    let oy = qy.max(0) as i64;
    // Along the straight edges the distance is just the offset; only the
    // corners need a square root.
    let outside = if ox == 0 {
        oy as i32
    } else if oy == 0 {
        ox as i32
    } else {
        isqrt((ox * ox + oy * oy) as u64) as i32
    };
    let inside = qx.max(qy).min(0);
    outside + inside - radius * 16
}

/// A pre-computed drop shadow for a rounded rectangle of a given size.
///
/// Two layers are combined: a wide, soft "ambient" shadow offset slightly
/// downwards and a tight "contact" shadow hugging the edge. Opacity falls
/// off with a smoothstep curve, which reads as a Gaussian blur. Computing
/// the mask once per window size keeps compositing cheap.
pub struct ShadowMask {
    /// Size of the shape the shadow belongs to.
    pub w: i32,
    pub h: i32,
    /// Extra pixels around the shape on each side (top/left/right; the
    /// bottom has `pad + offset`).
    pub pad: i32,
    pub offset: i32,
    /// Area inside the mask fully covered by the shape (never drawn).
    pub hidden: Rect,
    mw: i32,
    mh: i32,
    alpha: Vec<u8>,
}

fn smooth_falloff(d16: i32, blur: i32) -> u32 {
    // d16: distance outside the shape in 1/16 px (negative = inside).
    if d16 <= 0 {
        return 255;
    }
    let span = blur * 16;
    if d16 >= span {
        return 0;
    }
    // 1 - smoothstep(t), t in 0..=1024
    let t = (d16 as i64 * 1024 / span as i64) as i64;
    let s = t * t / 1024 * (3072 - 2 * t) / 1024;
    ((1024 - s) * 255 / 1024) as u32
}

impl ShadowMask {
    pub fn new(w: i32, h: i32, radius: i32, blur: i32, offset: i32, ambient: u32, contact: u32) -> ShadowMask {
        let pad = blur;
        let mw = w + 2 * pad;
        let mh = h + 2 * pad + offset;
        let mut alpha = vec![0u8; (mw.max(0) * mh.max(0)) as usize];
        let shape = Rect::new(pad, pad, w, h);
        let ambient_rect = shape.offset(0, offset);
        let contact_rect = shape.offset(0, 1);
        let hidden = shape.inset(radius.max(2));
        for y in 0..mh {
            for x in 0..mw {
                if hidden.contains(x, y) {
                    continue;
                }
                let a = smooth_falloff(rrect_distance16(x, y, &ambient_rect, radius), blur) * ambient / 255
                    + smooth_falloff(rrect_distance16(x, y, &contact_rect, radius), 3) * contact / 255;
                alpha[(y * mw + x) as usize] = a.min(255) as u8;
            }
        }
        ShadowMask { w, h, pad, offset, hidden, mw, mh, alpha }
    }

    /// Screen area covered when the shape is at `r`.
    pub fn bounds(&self, r: Rect) -> Rect {
        Rect::new(r.x - self.pad, r.y - self.pad, self.mw, self.mh)
    }
}

/// A mutable view onto pixels with a clip rectangle and a drawing origin.
pub struct Canvas<'a> {
    data: &'a mut [u32],
    pub width: i32,
    pub height: i32,
    stride: usize,
    clip: Rect,
    ox: i32,
    oy: i32,
}

impl<'a> Canvas<'a> {
    pub fn new(data: &'a mut [u32], width: i32, height: i32, stride: usize) -> Canvas<'a> {
        Canvas { data, width, height, stride, clip: Rect::new(0, 0, width, height), ox: 0, oy: 0 }
    }

    /// Restrict drawing to `r` (in current coordinates) intersected with the
    /// existing clip. Returns the previous clip so it can be restored.
    pub fn push_clip(&mut self, r: Rect) -> Rect {
        let old = self.clip;
        self.clip = self.clip.intersect(&r.offset(self.ox, self.oy));
        old
    }

    pub fn restore_clip(&mut self, old: Rect) {
        self.clip = old;
    }

    /// Clip rectangle in current (translated) coordinates.
    pub fn clip(&self) -> Rect {
        self.clip.offset(-self.ox, -self.oy)
    }

    pub fn translate(&mut self, dx: i32, dy: i32) {
        self.ox += dx;
        self.oy += dy;
    }

    pub fn origin(&self) -> (i32, i32) {
        (self.ox, self.oy)
    }

    pub fn set_origin(&mut self, o: (i32, i32)) {
        self.ox = o.0;
        self.oy = o.1;
    }

    #[inline]
    fn idx(&self, x: i32, y: i32) -> usize {
        y as usize * self.stride + x as usize
    }

    /// Direct access to the pixels of `r` (in current coordinates) when it
    /// lies entirely inside the clip region: (buffer starting at r's
    /// top-left corner, row stride).
    pub fn raw_region(&mut self, r: Rect) -> Option<(&mut [u32], usize)> {
        let abs = r.offset(self.ox, self.oy);
        if r.is_empty() || abs.intersect(&self.clip) != abs {
            return None;
        }
        let start = self.idx(abs.x, abs.y);
        let end = self.idx(abs.x, abs.bottom() - 1) + abs.w as usize;
        Some((&mut self.data[start..end], self.stride))
    }

    /// Absolute rectangle clipped to the clip region.
    fn abs_clip(&self, r: Rect) -> Rect {
        r.offset(self.ox, self.oy).intersect(&self.clip)
    }

    pub fn fill_rect(&mut self, r: Rect, color: Color) {
        let c = self.abs_clip(r);
        if c.is_empty() {
            return;
        }
        let a = alpha(color);
        for y in c.y..c.bottom() {
            let start = self.idx(c.x, y);
            let row = &mut self.data[start..start + c.w as usize];
            if a == 255 {
                row.fill(color);
            } else {
                for p in row.iter_mut() {
                    *p = blend(*p, color, a);
                }
            }
        }
    }

    pub fn clear(&mut self, color: Color) {
        let r = Rect::new(-self.ox, -self.oy, self.width, self.height);
        self.fill_rect(r, color | 0xff00_0000);
    }

    #[inline]
    pub fn blend_pixel(&mut self, x: i32, y: i32, color: Color, coverage: u32) {
        let ax = x + self.ox;
        let ay = y + self.oy;
        if !self.clip.contains(ax, ay) {
            return;
        }
        let a = alpha(color) * coverage / 255;
        let i = self.idx(ax, ay);
        self.data[i] = blend(self.data[i], color, a);
    }

    /// Top-to-bottom gradient.
    pub fn fill_gradient_v(&mut self, r: Rect, top: Color, bottom: Color) {
        let c = self.abs_clip(r);
        if c.is_empty() {
            return;
        }
        for y in c.y..c.bottom() {
            let t = ((y - (r.y + self.oy)) * 255 / r.h.max(1)).clamp(0, 255) as u32;
            let col = mix(top, bottom, t);
            let row = Rect::new(c.x - self.ox, y - self.oy, c.w, 1);
            self.fill_rect(row, col);
        }
    }

    /// Antialiased filled rounded rectangle.
    pub fn fill_rounded_rect(&mut self, r: Rect, radius: i32, color: Color) {
        let radius = radius.min(r.w / 2).min(r.h / 2).max(0);
        if radius == 0 {
            return self.fill_rect(r, color);
        }
        let a = alpha(color);
        let abs = r.offset(self.ox, self.oy);
        let c = abs.intersect(&self.clip);
        if c.is_empty() {
            return;
        }
        let (x0, y0, x1, y1) = (abs.x, abs.y, abs.right(), abs.bottom());
        for y in c.y..c.bottom() {
            let in_band = y < y0 + radius || y >= y1 - radius;
            if !in_band {
                let start = self.idx(c.x, y);
                let row = &mut self.data[start..start + c.w as usize];
                if a == 255 {
                    row.fill(color);
                } else {
                    for p in row.iter_mut() {
                        *p = blend(*p, color, a);
                    }
                }
                continue;
            }
            for x in c.x..c.right() {
                let cov = rrect_coverage(x, y, x0, y0, x1, y1, radius);
                if cov == 0 {
                    continue;
                }
                let i = self.idx(x, y);
                self.data[i] = blend(self.data[i], color, a * cov / 256);
            }
        }
    }

    /// Antialiased rounded outline of width `w` (drawn inside `r`).
    pub fn stroke_rounded_rect(&mut self, r: Rect, radius: i32, w: i32, color: Color) {
        let radius = radius.min(r.w / 2).min(r.h / 2).max(0);
        let inner = r.inset(w);
        let iradius = (radius - w).max(0);
        let a = alpha(color);
        let abs = r.offset(self.ox, self.oy);
        let iabs = inner.offset(self.ox, self.oy);
        let c = abs.intersect(&self.clip);
        for y in c.y..c.bottom() {
            for x in c.x..c.right() {
                let outer = if radius > 0 {
                    rrect_coverage(x, y, abs.x, abs.y, abs.right(), abs.bottom(), radius)
                } else {
                    256
                };
                if outer == 0 {
                    continue;
                }
                let inside = if iabs.contains(x, y) {
                    if iradius > 0 {
                        rrect_coverage(x, y, iabs.x, iabs.y, iabs.right(), iabs.bottom(), iradius)
                    } else {
                        256
                    }
                } else {
                    0
                };
                let cov = outer.saturating_sub(inside);
                if cov == 0 {
                    continue;
                }
                let i = self.idx(x, y);
                self.data[i] = blend(self.data[i], color, a * cov / 256);
            }
        }
    }

    pub fn fill_circle(&mut self, cx: i32, cy: i32, radius: i32, color: Color) {
        self.fill_rounded_rect(Rect::new(cx - radius, cy - radius, radius * 2, radius * 2), radius, color);
    }

    /// Soft drop shadow around a rounded rectangle. Only pixels outside the
    /// shape are touched, since the shape itself is drawn on top.
    pub fn draw_shadow(&mut self, r: Rect, radius: i32, blur: i32, color: Color) {
        let blur = blur.max(1);
        let area = Rect::new(r.x - blur, r.y - blur, r.w + blur * 2, r.h + blur * 2);
        let c = self.abs_clip(area);
        if c.is_empty() {
            return;
        }
        let a = alpha(color);
        let abs = r.offset(self.ox, self.oy);
        let inner = abs.inset(radius);
        for y in c.y..c.bottom() {
            for x in c.x..c.right() {
                if inner.contains(x, y) {
                    continue;
                }
                let d = rrect_distance16(x, y, &abs, radius);
                if d <= -8 || d >= blur * 16 {
                    continue;
                }
                // Quadratic falloff from full strength at the edge to 0 at `blur`.
                let t = (blur * 16 - d.max(0)) as u32;
                let span = (blur * 16) as u32;
                let s = t * t / span * 255 / span;
                let i = self.idx(x, y);
                self.data[i] = blend(self.data[i], color, a * s / 255);
            }
        }
    }

    /// Draw a cached shadow for a shape placed at `r` (which must have the
    /// mask's size), in `color` with its alpha scaled by `opacity`.
    pub fn draw_shadow_mask(&mut self, m: &ShadowMask, r: Rect, color: Color, opacity: u32) {
        let area = m.bounds(r).offset(self.ox, self.oy);
        let c = area.intersect(&self.clip);
        if c.is_empty() {
            return;
        }
        let a = alpha(color) * opacity / 255;
        let hidden = m.hidden.offset(area.x, area.y);
        for y in c.y..c.bottom() {
            let row = ((y - area.y) * m.mw) as usize;
            // Skip the part of the row the shape itself covers.
            let (skip_from, skip_to) =
                if y >= hidden.y && y < hidden.bottom() { (hidden.x, hidden.right()) } else { (i32::MAX, i32::MAX) };
            let mut x = c.x;
            while x < c.right() {
                if x == skip_from {
                    x = skip_to.max(x + 1);
                    continue;
                }
                let m_a = m.alpha[row + (x - area.x) as usize] as u32;
                if m_a != 0 {
                    let i = self.idx(x, y);
                    self.data[i] = blend(self.data[i], color, a * m_a / 255);
                }
                x += 1;
            }
        }
    }

    /// Like `draw_shadow_mask` but stretched to a shape placed at `r` of
    /// any size (used while windows animate).
    pub fn draw_shadow_mask_scaled(&mut self, m: &ShadowMask, r: Rect, color: Color, opacity: u32) {
        if r.w <= 0 || r.h <= 0 {
            return;
        }
        let sx = r.w as i64 * 1024 / m.w.max(1) as i64;
        let sy = r.h as i64 * 1024 / m.h.max(1) as i64;
        let pad_x = (m.pad as i64 * sx / 1024) as i32;
        let pad_y = (m.pad as i64 * sy / 1024) as i32;
        let area = Rect::new(r.x - pad_x, r.y - pad_y, (m.mw as i64 * sx / 1024) as i32, (m.mh as i64 * sy / 1024) as i32);
        let abs = area.offset(self.ox, self.oy);
        let c = abs.intersect(&self.clip);
        if c.is_empty() || abs.w <= 0 || abs.h <= 0 {
            return;
        }
        let a = alpha(color) * opacity / 255;
        let inner = r.offset(self.ox, self.oy).inset(4);
        for y in c.y..c.bottom() {
            let my = (((y - abs.y) as i64 * m.mh as i64) / abs.h as i64).min(m.mh as i64 - 1) as i32;
            for x in c.x..c.right() {
                if inner.contains(x, y) {
                    continue;
                }
                let mx = (((x - abs.x) as i64 * m.mw as i64) / abs.w as i64).min(m.mw as i64 - 1) as i32;
                let m_a = m.alpha[(my * m.mw + mx) as usize] as u32;
                if m_a != 0 {
                    let i = self.idx(x, y);
                    self.data[i] = blend(self.data[i], color, a * m_a / 255);
                }
            }
        }
    }

    /// Copy a surface to `(x, y)`.
    pub fn blit(&mut self, src: &Surface, x: i32, y: i32) {
        self.blit_region(src, Rect::new(0, 0, src.w, src.h), x, y);
    }

    /// Draw raw opaque pixels (`sw` x `sh`, row stride `sw`) into `dst`,
    /// scaled with nearest-neighbour sampling when the sizes differ.
    pub fn blit_pixels(&mut self, src: &[u32], sw: i32, sh: i32, dst: Rect) {
        let c = self.abs_clip(dst);
        if c.is_empty() || sw <= 0 || sh <= 0 || src.len() < (sw * sh) as usize {
            return;
        }
        let (x0, y0) = (dst.x + self.ox, dst.y + self.oy);
        let fx = ((sw as u64) << 16) / dst.w as u64;
        let fy = ((sh as u64) << 16) / dst.h as u64;
        for row in 0..c.h {
            let sy = ((((c.y + row - y0) as u64) * fy) >> 16).min(sh as u64 - 1) as usize;
            let srow = &src[sy * sw as usize..(sy + 1) * sw as usize];
            let d = self.idx(c.x, c.y + row);
            let out = &mut self.data[d..d + c.w as usize];
            if dst.w == sw {
                let sx = (c.x - x0) as usize;
                for (o, &p) in out.iter_mut().zip(&srow[sx..sx + c.w as usize]) {
                    *o = p | 0xff00_0000;
                }
            } else {
                for (i, o) in out.iter_mut().enumerate() {
                    let sx = ((((c.x - x0 + i as i32) as u64) * fx) >> 16).min(sw as u64 - 1) as usize;
                    *o = srow[sx] | 0xff00_0000;
                }
            }
        }
    }

    /// Copy part of a surface to `(x, y)`.
    pub fn blit_region(&mut self, src: &Surface, from: Rect, x: i32, y: i32) {
        let dst = Rect::new(x, y, from.w, from.h);
        let c = self.abs_clip(dst);
        if c.is_empty() {
            return;
        }
        let sx = from.x + (c.x - (x + self.ox));
        let sy = from.y + (c.y - (y + self.oy));
        for row in 0..c.h {
            let s = ((sy + row) * src.w + sx) as usize;
            let d = self.idx(c.x, c.y + row);
            self.data[d..d + c.w as usize].copy_from_slice(&src.data[s..s + c.w as usize]);
        }
    }

    /// Copy a surface with its corners rounded (antialiased against what is
    /// already on the canvas).
    pub fn blit_rounded(&mut self, src: &Surface, x: i32, y: i32, radius: i32) {
        let dst = Rect::new(x, y, src.w, src.h);
        let abs = dst.offset(self.ox, self.oy);
        let c = abs.intersect(&self.clip);
        if c.is_empty() {
            return;
        }
        let radius = radius.min(src.w / 2).min(src.h / 2).max(0);
        for py in c.y..c.bottom() {
            let sy = py - abs.y;
            let in_band = py < abs.y + radius || py >= abs.bottom() - radius;
            if !in_band {
                let s = (sy * src.w + (c.x - abs.x)) as usize;
                let d = self.idx(c.x, py);
                self.data[d..d + c.w as usize].copy_from_slice(&src.data[s..s + c.w as usize]);
                continue;
            }
            for px in c.x..c.right() {
                let cov = rrect_coverage(px, py, abs.x, abs.y, abs.right(), abs.bottom(), radius);
                if cov == 0 {
                    continue;
                }
                let sp = src.data[(sy * src.w + (px - abs.x)) as usize];
                let i = self.idx(px, py);
                self.data[i] = if cov >= 256 { sp } else { blend(self.data[i], sp, cov * 255 / 256) };
            }
        }
    }

    /// Draw `src` scaled into `dst` (nearest-neighbour), with a global
    /// opacity (0..=255) and rounded corners. Used for window animations.
    pub fn blit_scaled(&mut self, src: &Surface, dst: Rect, opacity: u32, radius: i32) {
        if dst.is_empty() || opacity == 0 {
            return;
        }
        let abs = dst.offset(self.ox, self.oy);
        let c = abs.intersect(&self.clip);
        if c.is_empty() {
            return;
        }
        let radius = radius.min(dst.w / 2).min(dst.h / 2).max(0);
        let step_x = ((src.w as i64) << 16) / dst.w as i64;
        let step_y = ((src.h as i64) << 16) / dst.h as i64;
        for py in c.y..c.bottom() {
            let sy = (((py - abs.y) as i64 * step_y) >> 16).min(src.h as i64 - 1) as i32;
            let row = (sy * src.w) as usize;
            let in_band = py < abs.y + radius || py >= abs.bottom() - radius;
            let mut sx_fp = (c.x - abs.x) as i64 * step_x;
            for px in c.x..c.right() {
                let sx = ((sx_fp >> 16) as i32).min(src.w - 1);
                sx_fp += step_x;
                let cov = if in_band { rrect_coverage(px, py, abs.x, abs.y, abs.right(), abs.bottom(), radius) } else { 256 };
                if cov == 0 {
                    continue;
                }
                let a = opacity * cov / 256;
                let i = self.idx(px, py);
                self.data[i] = blend(self.data[i], src.data[row + sx as usize], a);
            }
        }
    }

    pub fn hline(&mut self, x: i32, y: i32, w: i32, color: Color) {
        self.fill_rect(Rect::new(x, y, w, 1), color);
    }

    pub fn vline(&mut self, x: i32, y: i32, h: i32, color: Color) {
        self.fill_rect(Rect::new(x, y, 1, h), color);
    }

    /// Draw text with its baseline at `y`. Returns the x after the text.
    pub fn draw_text(&mut self, font: &Font, x: i32, y: i32, text: &str, color: Color) -> i32 {
        let mut pen = x * 16;
        let a = alpha(color);
        for ch in text.chars() {
            let Some(g) = font.glyph(ch) else {
                pen += font.fallback_advance();
                continue;
            };
            let gx = (pen + 8) / 16 + g.bearing_x as i32;
            let gy = y - g.bearing_top as i32;
            let r = Rect::new(gx, gy, g.w as i32, g.h as i32);
            let c = self.abs_clip(r);
            if !c.is_empty() {
                let bmp = font.bitmap(g);
                for yy in c.y..c.bottom() {
                    let row = (yy - (gy + self.oy)) as usize * g.w as usize;
                    for xx in c.x..c.right() {
                        let cov = bmp[row + (xx - (gx + self.ox)) as usize] as u32;
                        if cov != 0 {
                            let i = self.idx(xx, yy);
                            self.data[i] = blend(self.data[i], color, a * cov / 255);
                        }
                    }
                }
            }
            pen += g.advance as i32;
        }
        (pen + 8) / 16
    }

    /// Draw text, truncating with an ellipsis to fit `max_w`.
    pub fn draw_text_clipped(&mut self, font: &Font, x: i32, y: i32, text: &str, max_w: i32, color: Color) {
        if font.measure(text) <= max_w {
            self.draw_text(font, x, y, text, color);
            return;
        }
        let ell = "\u{2026}";
        let ell_w = font.measure(ell);
        let mut end = 0;
        let mut width = 0;
        for (i, ch) in text.char_indices() {
            let w = font.advance16(ch);
            if (width + w + 8) / 16 + ell_w > max_w {
                break;
            }
            width += w;
            end = i + ch.len_utf8();
        }
        let nx = self.draw_text(font, x, y, &text[..end], color);
        self.draw_text(font, nx, y, ell, color);
    }

    /// Draw text centred horizontally in `r` and vertically on its middle.
    pub fn draw_text_centered(&mut self, font: &Font, r: Rect, text: &str, color: Color) {
        let w = font.measure(text);
        let x = r.x + (r.w - w) / 2;
        let y = r.y + (r.h + font.ascent - font.descent) / 2;
        self.draw_text(font, x, y, text, color);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blend_extremes() {
        assert_eq!(blend(0xff000000, 0xffffffff, 255), 0xffffffff);
        assert_eq!(blend(0xff123456, 0xffffffff, 0), 0xff123456);
        let half = blend(0xff000000, 0xffffffff, 128);
        assert!((half & 0xff) >= 127 && (half & 0xff) <= 129);
    }

    #[test]
    fn isqrt_works() {
        for n in [0u64, 1, 2, 3, 4, 15, 16, 17, 1 << 40, 999_999_999] {
            let r = isqrt(n);
            assert!(r * r <= n && (r + 1) * (r + 1) > n, "{n}");
        }
    }

    #[test]
    fn rounded_rect_corners_are_antialiased() {
        let mut s = Surface::new(40, 40, 0);
        s.canvas().fill_rounded_rect(Rect::new(0, 0, 40, 40), 10, rgb(255, 255, 255));
        // Centre fully covered, extreme corner empty, corner edge partial.
        assert_eq!(s.pixel(20, 20), 0xffffffff);
        assert_eq!(s.pixel(0, 0) & 0xff, 0);
        let edge = (0..10).map(|i| s.pixel(i, 3) & 0xff).find(|&v| v > 0 && v < 255);
        assert!(edge.is_some(), "expected a partially covered pixel on the curve");
    }

    #[test]
    fn clipping_is_respected() {
        let mut s = Surface::new(10, 10, 0);
        let mut c = s.canvas();
        let old = c.push_clip(Rect::new(2, 2, 3, 3));
        c.fill_rect(Rect::new(0, 0, 10, 10), rgb(1, 2, 3));
        c.restore_clip(old);
        assert_eq!(s.pixel(0, 0), 0xff000000);
        assert_eq!(s.pixel(3, 3), rgb(1, 2, 3));
        assert_eq!(s.pixel(5, 5), 0xff000000);
    }

    #[test]
    fn shadow_fades_out_within_blur() {
        let mut s = Surface::new(100, 100, rgb(255, 255, 255));
        s.canvas().draw_shadow(Rect::new(30, 30, 40, 40), 8, 10, rgba(0, 0, 0, 255));
        // Just outside the edge: dark; beyond the blur radius: untouched.
        assert!(s.pixel(50, 71) & 0xff < 200);
        assert_eq!(s.pixel(50, 85), 0xffffffff);
        assert_eq!(s.pixel(5, 5), 0xffffffff);
    }

    #[test]
    fn scaled_blit_covers_target() {
        let src = Surface::new(10, 10, rgb(200, 0, 0));
        let mut dst = Surface::new(40, 40, rgb(0, 0, 0));
        dst.canvas().blit_scaled(&src, Rect::new(5, 5, 20, 20), 255, 0);
        assert_eq!(dst.pixel(5, 5), rgb(200, 0, 0));
        assert_eq!(dst.pixel(24, 24), rgb(200, 0, 0));
        assert_eq!(dst.pixel(25, 25), rgb(0, 0, 0));
        let mut half = Surface::new(4, 4, rgb(0, 0, 0));
        half.canvas().blit_scaled(&src, Rect::new(0, 0, 4, 4), 128, 0);
        assert!((half.pixel(1, 1) >> 16 & 0xff) > 90 && (half.pixel(1, 1) >> 16 & 0xff) < 110);
    }

    #[test]
    fn shadow_mask_is_soft_and_continuous() {
        let m = ShadowMask::new(100, 60, 12, 24, 6, 255, 120);
        let r = Rect::new(40, 40, 100, 60);
        let mut s = Surface::new(200, 200, rgb(255, 255, 255));
        s.canvas().draw_shadow_mask(&m, r, rgba(0, 0, 0, 255), 255);
        // Directly below the shape: darkest; fading smoothly further down.
        let below: Vec<u32> = (100..130).map(|y| s.pixel(90, y) & 0xff).collect();
        assert!(below[0] < 90, "no shadow right under the edge: {:?}", below);
        assert!(below.windows(2).all(|w| w[1] >= w[0]), "shadow must lighten monotonically: {:?}", below);
        // No hard jumps between neighbouring pixels.
        assert!(below.windows(2).all(|w| w[1] - w[0] < 40), "{:?}", below);
        // The shadow spreads to the sides and top too, but less.
        assert!(s.pixel(35, 70) & 0xff < 255 && s.pixel(90, 35) & 0xff < 255);
        assert!(s.pixel(90, 35) & 0xff > s.pixel(90, 104) & 0xff);
        // Far away: untouched.
        assert_eq!(s.pixel(5, 5), 0xffffffff);
    }

    #[test]
    fn text_renders() {
        let font = Font::parse(include_bytes!("../../../assets/fonts/sans-13.mfnt")).unwrap();
        assert!(font.measure("Hello") > 20);
        let mut s = Surface::new(80, 20, rgb(255, 255, 255));
        s.canvas().draw_text(&font, 2, 14, "Hello", rgb(0, 0, 0));
        assert!(s.data.iter().any(|&p| p & 0xff < 100));
    }
}
