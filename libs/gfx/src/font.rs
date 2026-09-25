//! Pre-rendered glyph atlases (`.mfnt`, produced by `tools/mkfont.py`).

use alloc::vec::Vec;

#[derive(Clone, Copy, Debug)]
pub struct Glyph {
    pub codepoint: u32,
    /// Advance in 1/16 pixel.
    pub advance: u16,
    pub bearing_x: i16,
    /// Pixels from the top of the bitmap to the baseline.
    pub bearing_top: i16,
    pub w: u16,
    pub h: u16,
    offset: u32,
}

pub struct Font {
    pub ascent: i32,
    pub descent: i32,
    pub line_height: i32,
    glyphs: Vec<Glyph>,
    ascii: [u16; 128],
    bitmaps: &'static [u8],
}

fn u16le(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
fn i16le(b: &[u8], o: usize) -> i16 {
    u16le(b, o) as i16
}
fn u32le(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

impl Font {
    pub fn parse(data: &'static [u8]) -> Option<Font> {
        if data.len() < 16 || &data[..4] != b"MFNT" || u16le(data, 4) != 1 {
            return None;
        }
        let count = u16le(data, 6) as usize;
        let ascent = i16le(data, 8) as i32;
        let descent = i16le(data, 10) as i32;
        let line_height = i16le(data, 12) as i32;
        let table = 16;
        let bitmaps_at = table + count * 18;
        if data.len() < bitmaps_at {
            return None;
        }
        let mut glyphs = Vec::with_capacity(count);
        for i in 0..count {
            let o = table + i * 18;
            glyphs.push(Glyph {
                codepoint: u32le(data, o),
                advance: u16le(data, o + 4),
                bearing_x: i16le(data, o + 6),
                bearing_top: i16le(data, o + 8),
                w: u16le(data, o + 10),
                h: u16le(data, o + 12),
                offset: u32le(data, o + 14),
            });
        }
        glyphs.sort_by_key(|g| g.codepoint);
        let bitmaps = &data[bitmaps_at..];
        for g in &glyphs {
            if g.offset as usize + g.w as usize * g.h as usize > bitmaps.len() {
                return None;
            }
        }
        let mut ascii = [u16::MAX; 128];
        for (i, g) in glyphs.iter().enumerate() {
            if g.codepoint < 128 {
                ascii[g.codepoint as usize] = i as u16;
            }
        }
        Some(Font { ascent, descent, line_height, glyphs, ascii, bitmaps })
    }

    pub fn glyph(&self, ch: char) -> Option<&Glyph> {
        let c = ch as u32;
        if c < 128 {
            let i = self.ascii[c as usize];
            return if i == u16::MAX { None } else { Some(&self.glyphs[i as usize]) };
        }
        self.glyphs.binary_search_by_key(&c, |g| g.codepoint).ok().map(|i| &self.glyphs[i])
    }

    pub fn bitmap(&self, g: &Glyph) -> &[u8] {
        let start = g.offset as usize;
        &self.bitmaps[start..start + g.w as usize * g.h as usize]
    }

    /// Advance used for characters the font lacks (1/16 px).
    pub fn fallback_advance(&self) -> i32 {
        self.glyph('?').map(|g| g.advance as i32).unwrap_or(8 * 16)
    }

    /// Advance of one character in 1/16 px.
    pub fn advance16(&self, ch: char) -> i32 {
        self.glyph(ch).map(|g| g.advance as i32).unwrap_or_else(|| self.fallback_advance())
    }

    /// Width of `text` in pixels.
    pub fn measure(&self, text: &str) -> i32 {
        let w: i32 = text.chars().map(|c| self.advance16(c)).sum();
        (w + 8) / 16
    }

    /// Pixel x offset of each character boundary (len = chars + 1).
    pub fn caret_positions(&self, text: &str) -> Vec<i32> {
        let mut out = Vec::with_capacity(text.len() + 1);
        let mut pen = 0;
        out.push(0);
        for c in text.chars() {
            pen += self.advance16(c);
            out.push((pen + 8) / 16);
        }
        out
    }
}
