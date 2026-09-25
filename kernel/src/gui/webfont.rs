//! TrueType text for the browser (the `ab_glyph` crate: glyphs are parsed
//! and rasterised lazily, the first time they are drawn). DejaVu Sans,
//! Sans Bold, Serif and Mono are built in; pages add their own fonts with
//! `@font-face` (TTF, OTF and WOFF 1.0).

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;

use ab_glyph::{point, Font, FontArc, PxScale, ScaleFont};
use gfx::Canvas;
use web::layout::FontSpec;

use crate::sync::Spin;

struct Face {
    /// Lower-case family name ("dejavu sans", "roboto"...).
    family: String,
    bold: bool,
    italic: bool,
    font: FontArc,
}

/// A rendered glyph: offset from the pen position (left, top relative to
/// the baseline), size and coverage.
struct Glyph {
    left: i32,
    top: i32,
    w: i32,
    h: i32,
    bitmap: Vec<u8>,
}

/// Scale for a CSS font size (the em box) in pixels.
fn scale_for(font: &FontArc, px: f32) -> PxScale {
    let upem = font.units_per_em().unwrap_or(1000.0);
    let h = font.ascent_unscaled() - font.descent_unscaled();
    PxScale::from(px * h / upem)
}

struct Store {
    faces: Vec<Face>,
    /// (face, glyph char, px) -> rendered glyph.
    glyphs: BTreeMap<(u16, char, u16), Glyph>,
    /// (face, char, px) -> advance in 1/64 px.
    advances: BTreeMap<(u16, char, u16), i32>,
    /// (family id, bold, italic, mono) -> face index.
    resolved: BTreeMap<(u32, bool, bool, bool), u16>,
}

static STORE: Spin<Option<Store>> = Spin::new(None);

const SANS: u16 = 0;
const SANS_BOLD: u16 = 1;
const SERIF: u16 = 2;
const MONO: u16 = 3;

fn with<R>(f: impl FnOnce(&mut Store) -> R) -> R {
    let mut g = STORE.lock();
    if g.is_none() {
        let load = |data: &'static [u8], family: &str, bold: bool| Face {
            family: String::from(family),
            bold,
            italic: false,
            font: FontArc::try_from_slice(data).expect("built-in font"),
        };
        *g = Some(Store {
            faces: alloc::vec![
                load(include_bytes!("../../../assets/fonts/ttf/DejaVuSans.ttf"), "dejavu sans", false),
                load(include_bytes!("../../../assets/fonts/ttf/DejaVuSans-Bold.ttf"), "dejavu sans", true),
                load(include_bytes!("../../../assets/fonts/ttf/DejaVuSerif.ttf"), "dejavu serif", false),
                load(include_bytes!("../../../assets/fonts/ttf/DejaVuSansMono.ttf"), "dejavu sans mono", false),
            ],
            glyphs: BTreeMap::new(),
            advances: BTreeMap::new(),
            resolved: BTreeMap::new(),
        });
    }
    f(g.as_mut().unwrap())
}

/// Add a font a page downloaded (TTF/OTF, or WOFF 1.0).
pub fn add_web_font(family: &str, bold: bool, italic: bool, data: Vec<u8>) -> bool {
    let data = if data.starts_with(b"wOFF") {
        match woff_to_sfnt(&data) {
            Some(d) => d,
            None => return false,
        }
    } else {
        data
    };
    let Ok(font) = FontArc::try_from_vec(data) else { return false };
    with(|s| {
        s.faces.push(Face { family: family.trim().trim_matches(['"', '\'']).to_ascii_lowercase(), bold, italic, font });
        s.resolved.clear();
    });
    true
}

fn pick(s: &mut Store, spec: FontSpec) -> u16 {
    let key = (spec.family, spec.bold, spec.italic, spec.mono);
    if let Some(&i) = s.resolved.get(&key) {
        return i;
    }
    let names = web::layout::family_name(spec.family);
    let mut found = None;
    for name in names.split(',') {
        let name = name.trim().trim_matches(['"', '\'']).to_ascii_lowercase();
        let generic = match name.as_str() {
            "monospace" | "ui-monospace" | "courier" | "courier new" | "consolas" | "menlo" => Some(MONO),
            "serif" | "times" | "times new roman" | "georgia" | "ui-serif" => Some(SERIF),
            "sans-serif" | "system-ui" | "-apple-system" | "arial" | "helvetica" | "helvetica neue" | "segoe ui" | "ui-sans-serif" | "verdana" => {
                Some(if spec.bold { SANS_BOLD } else { SANS })
            }
            _ => None,
        };
        // A downloaded face with this family name (best style match).
        let web = s
            .faces
            .iter()
            .enumerate()
            .skip(4)
            .filter(|(_, f)| f.family == name)
            .max_by_key(|(_, f)| (f.bold == spec.bold) as u8 * 2 + (f.italic == spec.italic) as u8)
            .map(|(i, _)| i as u16);
        if let Some(w) = web {
            found = Some(w);
            break;
        }
        if let Some(g) = generic {
            found = Some(g);
            break;
        }
    }
    let i = found.unwrap_or(if spec.mono { MONO } else if spec.bold { SANS_BOLD } else { SANS });
    s.resolved.insert(key, i);
    i
}

fn px(spec: FontSpec) -> u16 {
    spec.size.clamp(6, 120)
}

/// Width of `text` in pixels.
pub fn measure(text: &str, spec: FontSpec) -> i32 {
    with(|s| {
        let f = pick(s, spec);
        let size = px(spec);
        let mut w = 0i32;
        for ch in text.chars() {
            w += advance(s, f, ch, size);
        }
        (w + 32) / 64
    })
}

fn advance(s: &mut Store, f: u16, ch: char, size: u16) -> i32 {
    if let Some(&a) = s.advances.get(&(f, ch, size)) {
        return a;
    }
    let face = &s.faces[f as usize].font;
    // Characters the page font lacks come from DejaVu Sans.
    let font = if face.glyph_id(ch).0 != 0 || ch == ' ' { face } else { &s.faces[SANS as usize].font };
    let scaled = font.as_scaled(scale_for(font, size as f32));
    let a = (scaled.h_advance(font.glyph_id(ch)) * 64.0) as i32;
    if s.advances.len() > 60_000 {
        s.advances.clear();
    }
    s.advances.insert((f, ch, size), a);
    a
}

/// (ascent, line height) in pixels.
pub fn line_metrics(spec: FontSpec) -> (i32, i32) {
    with(|s| {
        let f = pick(s, spec);
        let font = &s.faces[f as usize].font;
        let sf = font.as_scaled(scale_for(font, px(spec) as f32));
        (libm::ceilf(sf.ascent()) as i32, libm::ceilf(sf.ascent() - sf.descent() + sf.line_gap()) as i32)
    })
}

/// Draw `text` with its baseline at `y`; returns the x after the text.
pub fn draw(c: &mut Canvas, x: i32, y: i32, text: &str, spec: FontSpec, color: u32) -> i32 {
    with(|s| {
        let f = pick(s, spec);
        let size = px(spec);
        let mut pen = x * 64;
        for ch in text.chars() {
            let adv = advance(s, f, ch, size);
            let gx = (pen + 32) / 64;
            pen += adv;
            if ch == ' ' {
                continue;
            }
            let face_idx = if s.faces[f as usize].font.glyph_id(ch).0 != 0 { f } else { SANS };
            let key = (face_idx, ch, size);
            if !s.glyphs.contains_key(&key) {
                if s.glyphs.len() > 4000 {
                    s.glyphs.clear();
                }
                let font = &s.faces[face_idx as usize].font;
                let glyph = font.glyph_id(ch).with_scale_and_position(scale_for(font, size as f32), point(0.0, 0.0));
                let g = match font.outline_glyph(glyph) {
                    Some(og) => {
                        let b = og.px_bounds();
                        let (w, h) = ((b.max.x - b.min.x) as i32, (b.max.y - b.min.y) as i32);
                        let mut bitmap = alloc::vec![0u8; (w.max(0) * h.max(0)) as usize];
                        og.draw(|x, y, c| {
                            let i = (y as i32 * w + x as i32) as usize;
                            if i < bitmap.len() {
                                bitmap[i] = (c * 255.0).min(255.0) as u8;
                            }
                        });
                        Glyph { left: b.min.x as i32, top: b.min.y as i32, w, h, bitmap }
                    }
                    None => Glyph { left: 0, top: 0, w: 0, h: 0, bitmap: Vec::new() },
                };
                s.glyphs.insert(key, g);
            }
            let g = &s.glyphs[&key];
            let (w, h, bitmap) = (g.w, g.h, &g.bitmap);
            if w <= 0 || h <= 0 {
                continue;
            }
            let ox = gx + g.left;
            let oy = y + g.top;
            for row in 0..h {
                for col in 0..w {
                    let cov = bitmap[(row * w + col) as usize] as u32;
                    if cov > 8 {
                        c.blend_pixel(ox + col, oy + row, color, cov);
                    }
                }
            }
            // Fake bold for faces without a bold version.
            if spec.bold && !s.faces[face_idx as usize].bold && face_idx != SANS_BOLD {
                for row in 0..h {
                    for col in 0..w {
                        let cov = bitmap[(row * w + col) as usize] as u32;
                        if cov > 8 {
                            c.blend_pixel(ox + col + 1, oy + row, color, cov);
                        }
                    }
                }
            }
        }
        (pen + 32) / 64
    })
}

/// WOFF 1.0 -> plain sfnt (each table zlib-compressed or stored).
fn woff_to_sfnt(d: &[u8]) -> Option<Vec<u8>> {
    let be32 = |o: usize| -> Option<u32> { Some(u32::from_be_bytes(d.get(o..o + 4)?.try_into().ok()?)) };
    let be16 = |o: usize| -> Option<u16> { Some(u16::from_be_bytes(d.get(o..o + 2)?.try_into().ok()?)) };
    let flavor = be32(4)?;
    let n = be16(12)? as usize;
    let mut tables = Vec::new();
    for i in 0..n {
        let e = 44 + i * 20;
        let (tag, off, clen, olen, sum) = (be32(e)?, be32(e + 4)? as usize, be32(e + 8)? as usize, be32(e + 12)? as usize, be32(e + 16)?);
        let raw = d.get(off..off + clen)?;
        let data = if clen < olen { miniz_oxide::inflate::decompress_to_vec_zlib(raw).ok()? } else { raw.to_vec() };
        tables.push((tag, sum, data));
    }
    let mut out = Vec::new();
    out.extend_from_slice(&flavor.to_be_bytes());
    let mut p = 1u16;
    let mut log = 0u16;
    while p * 2 <= n as u16 {
        p *= 2;
        log += 1;
    }
    for v in [n as u16, p * 16, log, n as u16 * 16 - p * 16] {
        out.extend_from_slice(&v.to_be_bytes());
    }
    let mut offset = 12 + n * 16;
    for (tag, sum, data) in &tables {
        for v in [*tag, *sum, offset as u32, data.len() as u32] {
            out.extend_from_slice(&v.to_be_bytes());
        }
        offset += (data.len() + 3) & !3;
    }
    for (_, _, data) in &tables {
        out.extend_from_slice(data);
        while out.len() % 4 != 0 {
            out.push(0);
        }
    }
    Some(out)
}
