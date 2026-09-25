//! Image decoding for MayOS: PNG, JPEG (baseline + progressive) and BMP,
//! plus high-quality scaling. `no_std` + `alloc`, tested on the host
//! against Pillow.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

pub mod avi;
pub mod bmp;
pub mod inflate;
pub mod jpeg;
pub mod png;

use alloc::vec;
use alloc::vec::Vec;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageError {
    Unsupported,
    Truncated,
    Corrupt,
    TooLarge,
}

impl ImageError {
    pub fn as_str(&self) -> &'static str {
        match self {
            ImageError::Unsupported => "unsupported image format",
            ImageError::Truncated => "the image file is incomplete",
            ImageError::Corrupt => "the image file is damaged",
            ImageError::TooLarge => "the image is too large",
        }
    }
}

impl core::fmt::Display for ImageError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Decoded image: 0xAARRGGBB pixels, straight (not premultiplied) alpha.
#[derive(Clone)]
pub struct Image {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u32>,
}

impl Image {
    pub fn new(width: u32, height: u32) -> Image {
        Image { width, height, pixels: vec![0; (width * height) as usize] }
    }

    #[inline]
    pub fn set(&mut self, x: u32, y: u32, [r, g, b, a]: [u8; 4]) {
        self.pixels[(y * self.width + x) as usize] = (a as u32) << 24 | (r as u32) << 16 | (g as u32) << 8 | b as u32;
    }

    pub fn get(&self, x: u32, y: u32) -> u32 {
        self.pixels[(y * self.width + x) as usize]
    }

    /// Resize with box filtering when shrinking and bilinear filtering
    /// when enlarging.
    pub fn resized(&self, w: u32, h: u32) -> Image {
        let w = w.max(1);
        let h = h.max(1);
        if w < self.width / 2 || h < self.height / 2 {
            // Halve first for quality and speed, then finish.
            return self.half().resized(w, h);
        }
        let mut out = Image::new(w, h);
        let sx = ((self.width as u64) << 16) / w as u64;
        let sy = ((self.height as u64) << 16) / h as u64;
        for y in 0..h {
            let fy = ((y as u64 * sy + sy / 2) as i64 - 32768).max(0) as u64;
            let y0 = ((fy >> 16) as u32).min(self.height - 1);
            let y1 = (y0 + 1).min(self.height - 1);
            let wy = (fy & 0xffff) as u32 >> 8;
            for x in 0..w {
                let fx = ((x as u64 * sx + sx / 2) as i64 - 32768).max(0) as u64;
                let x0 = ((fx >> 16) as u32).min(self.width - 1);
                let x1 = (x0 + 1).min(self.width - 1);
                let wx = (fx & 0xffff) as u32 >> 8;
                let p = [self.get(x0, y0), self.get(x1, y0), self.get(x0, y1), self.get(x1, y1)];
                let mut px = 0u32;
                for shift in [0u32, 8, 16, 24] {
                    let c = |v: u32| (v >> shift) & 0xff;
                    let top = c(p[0]) * (256 - wx) + c(p[1]) * wx;
                    let bot = c(p[2]) * (256 - wx) + c(p[3]) * wx;
                    let v = (top * (256 - wy) + bot * wy) >> 16;
                    px |= v.min(255) << shift;
                }
                out.pixels[(y * w + x) as usize] = px;
            }
        }
        out
    }

    /// 2x2 box downscale.
    fn half(&self) -> Image {
        let w = (self.width / 2).max(1);
        let h = (self.height / 2).max(1);
        let mut out = Image::new(w, h);
        for y in 0..h {
            for x in 0..w {
                let (x0, y0) = ((x * 2).min(self.width - 1), (y * 2).min(self.height - 1));
                let (x1, y1) = ((x0 + 1).min(self.width - 1), (y0 + 1).min(self.height - 1));
                let p = [self.get(x0, y0), self.get(x1, y0), self.get(x0, y1), self.get(x1, y1)];
                let mut px = 0;
                for shift in [0u32, 8, 16, 24] {
                    let s: u32 = p.iter().map(|v| (v >> shift) & 0xff).sum();
                    px |= ((s + 2) / 4) << shift;
                }
                out.pixels[(y * w + x) as usize] = px;
            }
        }
        out
    }

    /// Scale to fill `w`x`h` completely, cropping the overflow (like a
    /// desktop wallpaper), composited over `background`.
    pub fn cover(&self, w: u32, h: u32, background: u32) -> Image {
        let scale_w = w as u64 * 65536 / self.width as u64;
        let scale_h = h as u64 * 65536 / self.height as u64;
        let s = scale_w.max(scale_h);
        let nw = ((self.width as u64 * s) >> 16).max(w as u64) as u32;
        let nh = ((self.height as u64 * s) >> 16).max(h as u64) as u32;
        let big = self.resized(nw, nh);
        let (ox, oy) = ((nw - w) / 2, (nh - h) / 2);
        let mut out = Image::new(w, h);
        for y in 0..h {
            for x in 0..w {
                out.pixels[(y * w + x) as usize] = over(big.get(x + ox, y + oy), background);
            }
        }
        out
    }

    /// Scale to fit inside `w`x`h` keeping the aspect ratio.
    pub fn fit(&self, w: u32, h: u32) -> Image {
        let scale_w = w as u64 * 65536 / self.width as u64;
        let scale_h = h as u64 * 65536 / self.height as u64;
        let s = scale_w.min(scale_h);
        let nw = ((self.width as u64 * s) >> 16).clamp(1, w as u64) as u32;
        let nh = ((self.height as u64 * s) >> 16).clamp(1, h as u64) as u32;
        self.resized(nw, nh)
    }
}

/// Composite a straight-alpha pixel over an opaque background.
pub fn over(px: u32, bg: u32) -> u32 {
    let a = px >> 24;
    if a == 255 {
        return px;
    }
    let mix = |s: u32| {
        let f = (px >> s) & 0xff;
        let b = (bg >> s) & 0xff;
        (f * a + b * (255 - a)) / 255
    };
    0xff00_0000 | mix(16) << 16 | mix(8) << 8 | mix(0)
}

pub fn is_image(data: &[u8]) -> bool {
    png::is_png(data) || jpeg::is_jpeg(data) || bmp::is_bmp(data)
}

/// Decode PNG, JPEG or BMP data (detected from the contents).
pub fn decode(data: &[u8]) -> Result<Image, ImageError> {
    if png::is_png(data) {
        png::decode(data)
    } else if jpeg::is_jpeg(data) {
        jpeg::decode(data)
    } else if bmp::is_bmp(data) {
        bmp::decode(data)
    } else {
        Err(ImageError::Unsupported)
    }
}
