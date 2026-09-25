//! PNG decoding: all colour types and bit depths, Adam7 interlacing,
//! palette and tRNS transparency.

use alloc::vec::Vec;

use crate::{Image, ImageError};

fn be32(b: &[u8], o: usize) -> u32 {
    u32::from_be_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

pub fn is_png(data: &[u8]) -> bool {
    data.len() >= 8 && data[..8] == [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]
}

fn paeth(a: u8, b: u8, c: u8) -> u8 {
    let p = a as i16 + b as i16 - c as i16;
    let pa = (p - a as i16).abs();
    let pb = (p - b as i16).abs();
    let pc = (p - c as i16).abs();
    if pa <= pb && pa <= pc {
        a
    } else if pb <= pc {
        b
    } else {
        c
    }
}

/// Reverse the per-scanline filters in place. `bpp` = bytes per pixel
/// (at least 1), `stride` = bytes per row without the filter byte.
fn unfilter(data: &[u8], out: &mut Vec<u8>, rows: usize, stride: usize, bpp: usize) -> Result<(), ImageError> {
    if data.len() < rows * (stride + 1) {
        return Err(ImageError::Truncated);
    }
    out.resize(rows * stride, 0);
    for y in 0..rows {
        let f = data[y * (stride + 1)];
        let src = &data[y * (stride + 1) + 1..(y + 1) * (stride + 1)];
        let (prev_rows, cur_rows) = out.split_at_mut(y * stride);
        let prev = if y > 0 { &prev_rows[(y - 1) * stride..] } else { &[][..] };
        let cur = &mut cur_rows[..stride];
        for x in 0..stride {
            let a = if x >= bpp { cur[x - bpp] } else { 0 };
            let b = if y > 0 { prev[x] } else { 0 };
            let c = if y > 0 && x >= bpp { prev[x - bpp] } else { 0 };
            cur[x] = match f {
                0 => src[x],
                1 => src[x].wrapping_add(a),
                2 => src[x].wrapping_add(b),
                3 => src[x].wrapping_add(((a as u16 + b as u16) / 2) as u8),
                4 => src[x].wrapping_add(paeth(a, b, c)),
                _ => return Err(ImageError::Corrupt),
            };
        }
    }
    Ok(())
}

pub fn decode(data: &[u8]) -> Result<Image, ImageError> {
    if !is_png(data) {
        return Err(ImageError::Unsupported);
    }
    let mut pos = 8;
    let (mut w, mut h, mut depth, mut ctype, mut interlace) = (0u32, 0u32, 0u8, 0u8, 0u8);
    let mut palette: Vec<[u8; 4]> = Vec::new();
    let mut trns_key: Option<[u16; 3]> = None;
    let mut idat = Vec::new();
    while pos + 8 <= data.len() {
        let len = be32(data, pos) as usize;
        let kind = &data[pos + 4..pos + 8];
        let body_start = pos + 8;
        if body_start + len > data.len() {
            return Err(ImageError::Truncated);
        }
        let body = &data[body_start..body_start + len];
        match kind {
            b"IHDR" if len >= 13 => {
                w = be32(body, 0);
                h = be32(body, 4);
                depth = body[8];
                ctype = body[9];
                interlace = body[12];
            }
            b"PLTE" => {
                palette = body.chunks_exact(3).map(|c| [c[0], c[1], c[2], 255]).collect();
            }
            b"tRNS" => match ctype {
                3 => {
                    for (i, &a) in body.iter().enumerate() {
                        if i < palette.len() {
                            palette[i][3] = a;
                        }
                    }
                }
                0 if len >= 2 => {
                    let g = u16::from_be_bytes([body[0], body[1]]);
                    trns_key = Some([g, g, g]);
                }
                2 if len >= 6 => {
                    trns_key = Some([
                        u16::from_be_bytes([body[0], body[1]]),
                        u16::from_be_bytes([body[2], body[3]]),
                        u16::from_be_bytes([body[4], body[5]]),
                    ]);
                }
                _ => {}
            },
            b"IDAT" => idat.extend_from_slice(body),
            b"IEND" => break,
            _ => {}
        }
        pos = body_start + len + 4;
    }
    if w == 0 || h == 0 || w > 16384 || h > 16384 {
        return Err(ImageError::Corrupt);
    }
    let channels = match ctype {
        0 => 1,
        2 => 3,
        3 => 1,
        4 => 2,
        6 => 4,
        _ => return Err(ImageError::Unsupported),
    };
    if !matches!(depth, 1 | 2 | 4 | 8 | 16) || (ctype == 3 && depth == 16) || (ctype != 0 && ctype != 3 && depth < 8) {
        return Err(ImageError::Unsupported);
    }
    let bits_pp = channels * depth as usize;
    let bpp = bits_pp.div_ceil(8).max(1);
    let row_bytes = |width: usize| (width * bits_pp).div_ceil(8);
    let expected = if interlace == 0 {
        h as usize * (row_bytes(w as usize) + 1)
    } else {
        h as usize * (row_bytes(w as usize) + 1) * 2 + 64
    };
    let raw = crate::inflate::zlib_decompress(&idat, expected + 1024).map_err(|_| ImageError::Corrupt)?;

    let mut img = Image::new(w, h);
    // Read sample `i` of a row at the file's bit depth, scaled to 8 bits
    // (and the raw value for colour-key comparison).
    let sample = |row: &[u8], i: usize| -> (u8, u16) {
        match depth {
            16 => {
                let v = u16::from_be_bytes([row[i * 2], row[i * 2 + 1]]);
                ((v >> 8) as u8, v)
            }
            8 => (row[i], row[i] as u16),
            d => {
                let bit = i * d as usize;
                let v = (row[bit / 8] >> (8 - d as usize - bit % 8)) & ((1 << d) - 1);
                let scaled = if ctype == 3 { v } else { (v as u16 * 255 / ((1 << d) - 1)) as u8 };
                (scaled, v as u16)
            }
        }
    };
    let put_row = |img: &mut Image, row: &[u8], y: usize, x0: usize, dx: usize, count: usize| {
        for k in 0..count {
            let x = x0 + k * dx;
            let s = k * channels;
            let rgba = match ctype {
                0 => {
                    let (g, raw) = sample(row, s);
                    let a = if trns_key.map(|t| t[0] == raw).unwrap_or(false) { 0 } else { 255 };
                    [g, g, g, a]
                }
                2 => {
                    let (r, rr) = sample(row, s);
                    let (g, rg) = sample(row, s + 1);
                    let (b, rb) = sample(row, s + 2);
                    let a = if trns_key.map(|t| t == [rr, rg, rb]).unwrap_or(false) { 0 } else { 255 };
                    [r, g, b, a]
                }
                3 => {
                    let (i, _) = sample(row, s);
                    palette.get(i as usize).copied().unwrap_or([0, 0, 0, 255])
                }
                4 => {
                    let (g, _) = sample(row, s);
                    let (a, _) = sample(row, s + 1);
                    [g, g, g, a]
                }
                _ => {
                    let (r, _) = sample(row, s);
                    let (g, _) = sample(row, s + 1);
                    let (b, _) = sample(row, s + 2);
                    let (a, _) = sample(row, s + 3);
                    [r, g, b, a]
                }
            };
            img.set(x as u32, y as u32, rgba);
        }
    };

    let mut buf = Vec::new();
    if interlace == 0 {
        let stride = row_bytes(w as usize);
        unfilter(&raw, &mut buf, h as usize, stride, bpp)?;
        for y in 0..h as usize {
            put_row(&mut img, &buf[y * stride..(y + 1) * stride], y, 0, 1, w as usize);
        }
    } else {
        // Adam7: (x0, y0, dx, dy) for the seven passes.
        const PASSES: [(usize, usize, usize, usize); 7] =
            [(0, 0, 8, 8), (4, 0, 8, 8), (0, 4, 4, 8), (2, 0, 4, 4), (0, 2, 2, 4), (1, 0, 2, 2), (0, 1, 1, 2)];
        let mut off = 0;
        for &(x0, y0, dx, dy) in &PASSES {
            let pw = (w as usize).saturating_sub(x0).div_ceil(dx);
            let ph = (h as usize).saturating_sub(y0).div_ceil(dy);
            if pw == 0 || ph == 0 {
                continue;
            }
            let stride = row_bytes(pw);
            let size = ph * (stride + 1);
            if off + size > raw.len() {
                return Err(ImageError::Truncated);
            }
            unfilter(&raw[off..off + size], &mut buf, ph, stride, bpp)?;
            for r in 0..ph {
                put_row(&mut img, &buf[r * stride..(r + 1) * stride], y0 + r * dy, x0, dx, pw);
            }
            off += size;
        }
    }
    Ok(img)
}
