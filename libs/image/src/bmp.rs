//! Windows BMP: 1/4/8-bit palette, 16/24/32-bit, uncompressed or
//! BI_BITFIELDS, bottom-up or top-down.

use crate::{Image, ImageError};

fn le16(b: &[u8], o: usize) -> u32 {
    u16::from_le_bytes([b[o], b[o + 1]]) as u32
}
fn le32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

pub fn is_bmp(data: &[u8]) -> bool {
    data.len() > 26 && &data[..2] == b"BM"
}

fn mask_value(px: u32, mask: u32) -> u8 {
    if mask == 0 {
        return 255;
    }
    let shift = mask.trailing_zeros();
    let bits = (mask >> shift).count_ones();
    let v = (px & mask) >> shift;
    if bits >= 8 { (v >> (bits - 8)) as u8 } else { (v * 255 / ((1 << bits) - 1)) as u8 }
}

pub fn decode(data: &[u8]) -> Result<Image, ImageError> {
    if !is_bmp(data) {
        return Err(ImageError::Unsupported);
    }
    let off = le32(data, 10) as usize;
    let hdr = le32(data, 14) as usize;
    if 14 + hdr > data.len() || hdr < 12 {
        return Err(ImageError::Truncated);
    }
    let (w, h_raw, bpp, comp) = if hdr == 12 {
        (le16(data, 18) as i32, le16(data, 20) as i16 as i32, le16(data, 24), 0)
    } else {
        (le32(data, 18) as i32, le32(data, 22) as i32, le16(data, 28), le32(data, 30))
    };
    let top_down = h_raw < 0;
    let h = h_raw.unsigned_abs() as i32;
    if w <= 0 || h <= 0 || w > 16384 || h > 16384 {
        return Err(ImageError::Corrupt);
    }
    if comp != 0 && comp != 3 && comp != 6 {
        return Err(ImageError::Unsupported); // RLE
    }
    let (mut rm, mut gm, mut bm, mut am) = match bpp {
        16 => (0x7c00, 0x03e0, 0x001f, 0),
        32 => (0x00ff_0000, 0x0000_ff00, 0x0000_00ff, 0),
        _ => (0, 0, 0, 0),
    };
    if comp == 3 || comp == 6 {
        let mo = if hdr >= 52 { 14 + 40 } else { 14 + hdr };
        if mo + 12 <= data.len() {
            rm = le32(data, mo);
            gm = le32(data, mo + 4);
            bm = le32(data, mo + 8);
            if (hdr >= 56 || comp == 6) && mo + 16 <= data.len() {
                am = le32(data, mo + 12);
            }
        }
    }
    let palette_at = 14 + hdr;
    let entry = if hdr == 12 { 3 } else { 4 };
    let stride = ((w as usize * bpp as usize).div_ceil(32)) * 4;
    if off + stride * h as usize > data.len() {
        return Err(ImageError::Truncated);
    }
    let mut img = Image::new(w as u32, h as u32);
    let mut any_alpha = false;
    for row in 0..h as usize {
        let y = if top_down { row } else { h as usize - 1 - row };
        let line = &data[off + row * stride..off + (row + 1) * stride];
        for x in 0..w as usize {
            let rgba = match bpp {
                1 | 4 | 8 => {
                    let bit = x * bpp as usize;
                    let idx = (line[bit / 8] >> (8 - bpp as usize - bit % 8)) & ((1u16 << bpp) - 1) as u8;
                    let p = palette_at + idx as usize * entry;
                    if p + 3 > data.len() {
                        return Err(ImageError::Corrupt);
                    }
                    [data[p + 2], data[p + 1], data[p], 255]
                }
                24 => [line[x * 3 + 2], line[x * 3 + 1], line[x * 3], 255],
                16 => {
                    let px = le16(line, x * 2);
                    [mask_value(px, rm), mask_value(px, gm), mask_value(px, bm), 255]
                }
                32 => {
                    let px = le32(line, x * 4);
                    let a = if am != 0 { mask_value(px, am) } else { 255 };
                    if am != 0 && a != 0 {
                        any_alpha = true;
                    }
                    [mask_value(px, rm), mask_value(px, gm), mask_value(px, bm), a]
                }
                _ => return Err(ImageError::Unsupported),
            };
            img.set(x as u32, y as u32, rgba);
        }
    }
    // Some writers set an alpha mask but leave it all zero: treat as opaque.
    if am != 0 && !any_alpha {
        for p in img.pixels.iter_mut() {
            *p |= 0xff00_0000;
        }
    }
    Ok(img)
}
