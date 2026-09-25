//! Painting `<canvas>` 2D drawing commands sent by page scripts (as a
//! JSON list of `[op, args...]`) into a surface.

use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use gfx::{Canvas, Rect, Surface};

/// A parsed JSON value (just what the command lists use).
#[derive(Debug, Clone)]
enum J {
    Num(f64),
    Str(String),
    Arr(Vec<J>),
    Other,
}

fn parse_json(s: &[u8], i: &mut usize) -> J {
    while *i < s.len() && s[*i].is_ascii_whitespace() {
        *i += 1;
    }
    if *i >= s.len() {
        return J::Other;
    }
    match s[*i] {
        b'[' => {
            *i += 1;
            let mut v = Vec::new();
            loop {
                while *i < s.len() && (s[*i].is_ascii_whitespace() || s[*i] == b',') {
                    *i += 1;
                }
                if *i >= s.len() || s[*i] == b']' {
                    *i += 1;
                    break;
                }
                v.push(parse_json(s, i));
            }
            J::Arr(v)
        }
        b'"' => {
            *i += 1;
            let mut out = Vec::new();
            while *i < s.len() && s[*i] != b'"' {
                if s[*i] == b'\\' && *i + 1 < s.len() {
                    *i += 1;
                    match s[*i] {
                        b'n' => out.push(b'\n'),
                        b't' => out.push(b'\t'),
                        b'u' if *i + 4 < s.len() => {
                            let h = core::str::from_utf8(&s[*i + 1..*i + 5]).ok().and_then(|h| u32::from_str_radix(h, 16).ok()).and_then(char::from_u32).unwrap_or('?');
                            let mut buf = [0u8; 4];
                            out.extend_from_slice(h.encode_utf8(&mut buf).as_bytes());
                            *i += 4;
                        }
                        c => out.push(c),
                    }
                } else {
                    out.push(s[*i]);
                }
                *i += 1;
            }
            *i += 1;
            J::Str(String::from_utf8_lossy(&out).into_owned())
        }
        b'-' | b'0'..=b'9' => {
            let st = *i;
            while *i < s.len() && matches!(s[*i], b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9') {
                *i += 1;
            }
            J::Num(core::str::from_utf8(&s[st..*i]).ok().and_then(|t| t.parse().ok()).unwrap_or(0.0))
        }
        _ => {
            while *i < s.len() && s[*i].is_ascii_alphabetic() {
                *i += 1;
            }
            J::Other
        }
    }
}

impl J {
    fn n(&self) -> f64 {
        match self {
            J::Num(n) => *n,
            _ => 0.0,
        }
    }
    fn s(&self) -> &str {
        match self {
            J::Str(s) => s,
            _ => "",
        }
    }
}

#[derive(Clone, Copy)]
struct Xf {
    tx: f64,
    ty: f64,
    sx: f64,
    sy: f64,
}

pub struct Painter<'a> {
    pub surface: &'a mut Surface,
    pub images: &'a BTreeMap<String, Surface>,
    /// Resolve an image `src` to its key in `images`.
    pub resolve: &'a dyn Fn(&str) -> String,
}

fn color(s: &str, alpha: f64) -> u32 {
    let c = web::css::parse_color(s).unwrap_or(0xff00_0000);
    let a = ((c >> 24) as f64 * alpha.clamp(0.0, 1.0)) as u32;
    (a << 24) | (c & 0x00ff_ffff)
}

impl Painter<'_> {
    pub fn run(&mut self, json: &str) {
        let mut i = 0;
        let J::Arr(ops) = parse_json(json.as_bytes(), &mut i) else { return };
        let mut xf = Xf { tx: 0.0, ty: 0.0, sx: 1.0, sy: 1.0 };
        let mut stack: Vec<Xf> = Vec::new();
        let mut path: Vec<Vec<(f64, f64)>> = Vec::new();
        let (w, h) = (self.surface.w, self.surface.h);
        for op in ops {
            let J::Arr(a) = op else { continue };
            let Some(name) = a.first().map(|x| x.s().to_string()) else { continue };
            let p = |k: usize| a.get(k).map(|v| v.n()).unwrap_or(0.0);
            let pt = |x: f64, y: f64, xf: &Xf| (x * xf.sx + xf.tx, y * xf.sy + xf.ty);
            let mut c = self.surface.canvas();
            match name.as_str() {
                "save" => stack.push(xf),
                "restore" => xf = stack.pop().unwrap_or(xf),
                "tr" => {
                    xf.tx += p(1) * xf.sx;
                    xf.ty += p(2) * xf.sy;
                }
                "sc" => {
                    xf.sx *= p(1);
                    xf.sy *= p(2);
                }
                "rt" => xf = Xf { tx: 0.0, ty: 0.0, sx: 1.0, sy: 1.0 },
                "fr" | "cr" | "sr" => {
                    let (x0, y0) = pt(p(1), p(2), &xf);
                    let (x1, y1) = pt(p(1) + p(3), p(2) + p(4), &xf);
                    let r = Rect::new(x0.min(x1) as i32, y0.min(y1) as i32, libm::ceil(libm::fabs((x1 - x0))) as i32, libm::ceil(libm::fabs((y1 - y0))) as i32);
                    match name.as_str() {
                        "fr" => {
                            let col = color(a.get(5).map(|v| v.s()).unwrap_or("#000"), p(6));
                            if col >> 24 == 255 {
                                c.fill_rect(r, col);
                            } else {
                                c.fill_rect(r, col);
                            }
                        }
                        "cr" => {
                            let rr = r.intersect(&Rect::new(0, 0, w, h));
                            for y in rr.y..rr.bottom() {
                                for x in rr.x..rr.right() {
                                    self.surface.data[(y * w + x) as usize] = 0;
                                }
                            }
                        }
                        _ => {
                            let col = color(a.get(5).map(|v| v.s()).unwrap_or("#000"), p(7));
                            let lw = (p(6) * xf.sx).max(1.0) as i32;
                            c.fill_rect(Rect::new(r.x, r.y, r.w, lw), col);
                            c.fill_rect(Rect::new(r.x, r.bottom() - lw, r.w, lw), col);
                            c.fill_rect(Rect::new(r.x, r.y, lw, r.h), col);
                            c.fill_rect(Rect::new(r.right() - lw, r.y, lw, r.h), col);
                        }
                    }
                }
                "bp" => path.clear(),
                "mt" => path.push(alloc::vec![pt(p(1), p(2), &xf)]),
                "lt" => {
                    let q = pt(p(1), p(2), &xf);
                    match path.last_mut() {
                        Some(sp) => sp.push(q),
                        None => path.push(alloc::vec![q]),
                    }
                }
                "cp" => {
                    if let Some(sp) = path.last_mut()
                        && let Some(&f) = sp.first()
                    {
                        sp.push(f);
                    }
                }
                "arc" => {
                    let (cx, cy, r, a0, a1, ccw) = (p(1), p(2), p(3), p(4), p(5), p(6) != 0.0);
                    let mut sweep = a1 - a0;
                    let tau = core::f64::consts::TAU;
                    if ccw {
                        if sweep > 0.0 {
                            sweep -= tau * libm::ceil(sweep / tau);
                        }
                    } else if sweep < 0.0 {
                        sweep += tau * libm::ceil(-sweep / tau);
                    }
                    sweep = sweep.clamp(-tau, tau);
                    let steps = ((r * xf.sx.abs()).max(4.0) * sweep.abs() / 4.0).clamp(8.0, 256.0) as usize;
                    let sp = match path.last_mut() {
                        Some(sp) => sp,
                        None => {
                            path.push(Vec::new());
                            path.last_mut().unwrap()
                        }
                    };
                    for k in 0..=steps {
                        let t = a0 + sweep * k as f64 / steps as f64;
                        sp.push(pt(cx + r * libm::cos(t), cy + r * libm::sin(t), &xf));
                    }
                }
                "fill" => {
                    let col = color(a.get(1).map(|v| v.s()).unwrap_or("#000"), p(2));
                    fill_polys(&mut c, &path, col, w, h);
                }
                "stroke" => {
                    let col = color(a.get(1).map(|v| v.s()).unwrap_or("#000"), p(3));
                    let lw = (p(2) * xf.sx).max(1.0);
                    for sp in &path {
                        for seg in sp.windows(2) {
                            line(&mut c, seg[0], seg[1], lw, col);
                        }
                    }
                }
                "ft" => {
                    let text = a.get(1).map(|v| v.s()).unwrap_or("");
                    let (x, y) = pt(p(2), p(3), &xf);
                    let col = color(a.get(4).map(|v| v.s()).unwrap_or("#000"), 1.0);
                    let font_s = a.get(5).map(|v| v.s()).unwrap_or("10px sans-serif");
                    let size = font_s.split_whitespace().find_map(|t| t.strip_suffix("px")).and_then(|t| t.parse::<f32>().ok()).unwrap_or(10.0);
                    let bold = font_s.contains("bold");
                    let family = font_s.split_once("px").map(|(_, r)| r.trim()).unwrap_or("sans-serif");
                    let spec = web::layout::FontSpec { family: web::layout::family_id(&family.to_ascii_lowercase()), size: size as u16, bold, italic: false, mono: font_s.contains("mono") };
                    let tw = super::webfont::measure(text, spec);
                    let (ascent, lh) = super::webfont::line_metrics(spec);
                    let align = a.get(6).map(|v| v.s()).unwrap_or("start");
                    let x = match align {
                        "center" => x as i32 - tw / 2,
                        "right" | "end" => x as i32 - tw,
                        _ => x as i32,
                    };
                    let descent = ascent - lh;
                    let base = match a.get(7).map(|v| v.s()).unwrap_or("alphabetic") {
                        "top" | "hanging" => y as i32 + ascent,
                        "middle" => y as i32 + (ascent + descent) / 2,
                        "bottom" => y as i32 + descent,
                        _ => y as i32,
                    };
                    super::webfont::draw(&mut c, x, base, text, spec, col);
                }
                "di" => {
                    let key = (self.resolve)(a.get(1).map(|v| v.s()).unwrap_or(""));
                    let Some(img) = self.images.get(&key) else { continue };
                    let (mut dw, mut dh) = (p(4), p(5));
                    if dw < 0.0 {
                        dw = img.w as f64;
                        dh = img.h as f64;
                    }
                    let (x0, y0) = pt(p(2), p(3), &xf);
                    let dst = Rect::new(x0 as i32, y0 as i32, (dw * xf.sx) as i32, (dh * xf.sy) as i32);
                    if p(6) >= 0.0 && p(8) > 0.0 {
                        // Source rectangle: copy it out first.
                        let (sx, sy, sw, sh) = (p(6) as i32, p(7) as i32, p(8) as i32, p(9) as i32);
                        let mut part = Surface::new(sw.max(1), sh.max(1), 0);
                        for yy in 0..sh.max(1) {
                            for xx in 0..sw.max(1) {
                                let (ix, iy) = (sx + xx, sy + yy);
                                if ix >= 0 && iy >= 0 && ix < img.w && iy < img.h {
                                    part.data[(yy * part.w + xx) as usize] = img.data[(iy * img.w + ix) as usize];
                                }
                            }
                        }
                        c.blit_scaled(&part, dst, 255, 0);
                    } else {
                        c.blit_scaled(img, dst, 255, 0);
                    }
                }
                _ => {}
            }
        }
    }
}

/// Even-odd scanline fill of polygons.
fn fill_polys(c: &mut Canvas, polys: &[Vec<(f64, f64)>], col: u32, w: i32, h: i32) {
    let (mut y0, mut y1) = (f64::MAX, f64::MIN);
    for p in polys {
        for &(_, y) in p {
            y0 = y0.min(y);
            y1 = y1.max(y);
        }
    }
    if y0 > y1 {
        return;
    }
    let (ys, ye) = ((libm::floor(y0) as i32).max(0), (libm::ceil(y1) as i32).min(h));
    let mut xs: Vec<f64> = Vec::new();
    for y in ys..ye {
        let fy = y as f64 + 0.5;
        xs.clear();
        for p in polys {
            let n = p.len();
            for i in 0..n {
                let (a, b) = (p[i], p[(i + 1) % n]);
                if (a.1 <= fy) != (b.1 <= fy) {
                    xs.push(a.0 + (fy - a.1) * (b.0 - a.0) / (b.1 - a.1));
                }
            }
        }
        xs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(core::cmp::Ordering::Equal));
        for pair in xs.chunks_exact(2) {
            let (xa, xb) = ((libm::round(pair[0]) as i32).max(0), (libm::round(pair[1]) as i32).min(w));
            if xb > xa {
                c.fill_rect(Rect::new(xa, y, xb - xa, 1), col);
            }
        }
    }
}

fn line(c: &mut Canvas, a: (f64, f64), b: (f64, f64), lw: f64, col: u32) {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let len = libm::sqrt(dx * dx + dy * dy).max(1.0);
    let steps = libm::ceil(len) as i32;
    let s = lw.max(1.0) as i32;
    for k in 0..=steps {
        let t = k as f64 / steps as f64;
        let (x, y) = (a.0 + dx * t, a.1 + dy * t);
        c.fill_rect(Rect::new(x as i32 - s / 2, y as i32 - s / 2, s, s), col);
    }
}
