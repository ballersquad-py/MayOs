//! Procedurally drawn icons (no image decoders yet: those arrive in
//! Phase 3). Every icon is drawn into a square of side `s` at `(x, y)`.

use crate::{rgb, with_alpha, Canvas, Color, Rect};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Icon {
    Folder,
    File,
    TextFile,
    Program,
    Image,
    Terminal,
    Explorer,
    Editor,
    Info,
    Drive,
    Home,
    Settings,
    Video,
    Music,
}

pub fn draw(c: &mut Canvas, icon: Icon, x: i32, y: i32, s: i32) {
    match icon {
        Icon::Folder => folder(c, x, y, s),
        Icon::File => page(c, x, y, s, rgb(0xf4, 0xf6, 0xfa), None),
        Icon::TextFile => page(c, x, y, s, rgb(0xf4, 0xf6, 0xfa), Some(rgb(0x8a, 0x95, 0xa8))),
        Icon::Image => {
            page(c, x, y, s, rgb(0xf4, 0xf6, 0xfa), None);
            let m = s / 5;
            c.fill_rounded_rect(Rect::new(x + m + s / 16, y + s / 2, s - 2 * m - s / 8, s / 3), s / 12, rgb(0x4c, 0xaf, 0x7d));
            c.fill_circle(x + s / 2 + s / 10, y + s / 3 + s / 16, s / 10, rgb(0xf5, 0xb9, 0x42));
        }
        Icon::Program => program(c, x, y, s),
        Icon::Terminal => terminal(c, x, y, s),
        Icon::Explorer => {
            app_tile(c, x, y, s, rgb(0x3d, 0x8b, 0xfd), rgb(0x1f, 0x5f, 0xd6));
            folder(c, x + s / 5, y + s / 5, s * 3 / 5);
        }
        Icon::Editor => {
            app_tile(c, x, y, s, rgb(0xf7, 0xa9, 0x3b), rgb(0xe0, 0x7b, 0x1a));
            page(c, x + s / 4, y + s / 5, s / 2, rgb(0xff, 0xff, 0xff), Some(rgb(0xa0, 0xa8, 0xb8)));
        }
        Icon::Info => {
            app_tile(c, x, y, s, rgb(0x8e, 0x6c, 0xef), rgb(0x64, 0x45, 0xc9));
            let cx = x + s / 2;
            c.fill_circle(cx, y + s * 3 / 10, (s / 14).max(1), rgb(255, 255, 255));
            c.fill_rounded_rect(Rect::new(cx - s / 14, y + s * 2 / 5, s / 7, s * 2 / 5), s / 16, rgb(255, 255, 255));
        }
        Icon::Drive => {
            let r = Rect::new(x + s / 10, y + s / 3, s * 4 / 5, s / 3);
            c.fill_rounded_rect(r, s / 10, rgb(0x6b, 0x77, 0x8c));
            c.fill_rounded_rect(r.inset(1), s / 10, rgb(0x9a, 0xa5, 0xb8));
            c.fill_circle(x + s * 3 / 4, y + s / 2, (s / 16).max(1), rgb(0x4c, 0xd9, 0x64));
        }
        Icon::Video => {
            let r = Rect::new(x + s / 8, y + s / 5, s * 3 / 4, s * 3 / 5);
            c.fill_rounded_rect(r.offset(0, 1), s / 10, with_alpha(0x000000, 50));
            c.fill_rounded_rect(r, s / 10, rgb(0x2b, 0x2f, 0x3a));
            // Sprocket holes.
            let hole = (s / 14).max(2);
            let mut hx = r.x + hole;
            while hx + hole < r.right() - hole / 2 {
                c.fill_rect(Rect::new(hx, r.y + hole / 2 + 1, hole, hole), rgb(0xe8, 0xec, 0xf2));
                c.fill_rect(Rect::new(hx, r.bottom() - hole * 3 / 2 - 1, hole, hole), rgb(0xe8, 0xec, 0xf2));
                hx += hole * 2;
            }
            // Play triangle.
            let (cx, cy, t) = (x + s / 2, y + s / 2, (s / 7).max(2));
            for i in 0..t * 2 {
                let len = if i < t { i } else { 2 * t - i };
                c.fill_rect(Rect::new(cx - t / 2, cy - t + i, len.max(1), 1), rgb(0xff, 0x5a, 0x5f));
            }
        }
        Icon::Music => {
            page(c, x, y, s, rgb(0xfd, 0xf1, 0xf6), None);
            let col = rgb(0xe0, 0x4f, 0x92);
            let (cx, cy) = (x + s / 2 - s / 12, y + s * 2 / 3);
            c.fill_circle(cx, cy, (s / 10).max(2), col);
            c.fill_rect(Rect::new(cx + s / 12, y + s / 3, (s / 20).max(1), cy - y - s / 3), col);
            c.fill_rect(Rect::new(cx + s / 12, y + s / 3, s / 6, (s / 14).max(1)), col);
        }
        Icon::Settings => {
            app_tile(c, x, y, s, rgb(0x9a, 0xa3, 0xb2), rgb(0x5f, 0x68, 0x78));
            gear(c, x + s / 2, y + s / 2, s * 3 / 10, rgb(255, 255, 255), rgb(0x7d, 0x86, 0x96));
        }
        Icon::Home => {
            let body = Rect::new(x + s / 4, y + s * 9 / 20, s / 2, s * 2 / 5);
            c.fill_rounded_rect(body, s / 20, rgb(0x3d, 0x8b, 0xfd));
            // Roof as stacked rows.
            let top = y + s / 6;
            let rows = s * 3 / 10;
            for i in 0..rows {
                let half = (i * (s * 7 / 20)) / rows.max(1);
                c.fill_rect(Rect::new(x + s / 2 - half, top + i, half * 2, 1), rgb(0x2a, 0x6f, 0xe0));
            }
            c.fill_rounded_rect(Rect::new(x + s / 2 - s / 12, y + s * 3 / 5, s / 6, s / 4), 2, rgb(255, 255, 255));
        }
    }
}

fn app_tile(c: &mut Canvas, x: i32, y: i32, s: i32, top: Color, bottom: Color) {
    let r = Rect::new(x, y, s, s);
    let old = c.push_clip(r);
    // Gradient tile with rounded corners: draw gradient rows through a
    // rounded mask by filling rounded rects of each row colour.
    let radius = s / 5;
    for i in 0..s {
        let t = (i * 255 / s.max(1)) as u32;
        let col = crate::mix(top, bottom, t);
        let row = Rect::new(x, y + i, s, 1);
        let o2 = c.push_clip(row);
        c.fill_rounded_rect(r, radius, col);
        c.restore_clip(o2);
    }
    c.stroke_rounded_rect(r, radius, 1, with_alpha(0x000000, 40));
    c.restore_clip(old);
}

fn folder(c: &mut Canvas, x: i32, y: i32, s: i32) {
    let back = rgb(0x4f, 0x9d, 0xf7);
    let front = rgb(0x7a, 0xb8, 0xff);
    let tab = Rect::new(x + s / 12, y + s / 6, s * 5 / 12, s / 5);
    c.fill_rounded_rect(tab, s / 16 + 1, back);
    let body = Rect::new(x + s / 12, y + s / 4, s * 10 / 12, s * 3 / 5);
    c.fill_rounded_rect(body, s / 12 + 1, back);
    let lid = Rect::new(x + s / 12, y + s / 3, s * 10 / 12, s * 31 / 60);
    c.fill_rounded_rect(lid, s / 12 + 1, front);
    c.hline(lid.x + s / 16, lid.y, lid.w - s / 8, with_alpha(0xffffff, 110));
}

fn page(c: &mut Canvas, x: i32, y: i32, s: i32, fill: Color, lines: Option<Color>) {
    let r = Rect::new(x + s / 5, y + s / 10, s * 3 / 5, s * 4 / 5);
    c.fill_rounded_rect(r.offset(0, 1), s / 14 + 1, with_alpha(0x000000, 45));
    c.fill_rounded_rect(r, s / 14 + 1, rgb(0xc5, 0xcc, 0xd8));
    c.fill_rounded_rect(r.inset(1), s / 14, fill);
    // Folded corner.
    let f = s / 6;
    c.fill_rect(Rect::new(r.right() - f, r.y, f, f), rgb(0xdc, 0xe2, 0xec));
    if let Some(col) = lines {
        let lx = r.x + s / 10;
        let lw = r.w - s / 5;
        let mut ly = r.y + s / 4;
        let step = (s / 9).max(2);
        let mut i = 0;
        while ly < r.bottom() - s / 10 {
            let w = if i % 3 == 2 { lw * 2 / 3 } else { lw };
            c.fill_rect(Rect::new(lx, ly, w, (s / 32).max(1)), col);
            ly += step;
            i += 1;
        }
    }
}

fn program(c: &mut Canvas, x: i32, y: i32, s: i32) {
    let r = Rect::new(x + s / 8, y + s / 6, s * 3 / 4, s * 2 / 3);
    c.fill_rounded_rect(r, s / 10, rgb(0x2d, 0x33, 0x40));
    c.fill_rounded_rect(Rect::new(r.x, r.y, r.w, s / 7), s / 10, rgb(0x5b, 0x67, 0x7d));
    let g = rgb(0x4c, 0xd9, 0x64);
    let px = r.x + s / 8;
    let py = r.y + s / 4;
    let t = (s / 20).max(1);
    for i in 0..s / 7 {
        c.fill_rect(Rect::new(px + i, py + i, t, t), g);
        c.fill_rect(Rect::new(px + i, py + 2 * (s / 7) - i, t, t), g);
    }
    c.fill_rect(Rect::new(px + s / 5, py + 2 * (s / 7), s / 5, t), g);
}

fn terminal(c: &mut Canvas, x: i32, y: i32, s: i32) {
    app_tile(c, x, y, s, rgb(0x3a, 0x40, 0x4f), rgb(0x1c, 0x20, 0x29));
    let g = rgb(0xe8, 0xec, 0xf2);
    let t = (s / 16).max(1);
    let px = x + s / 4;
    let py = y + s / 3;
    for i in 0..s / 7 {
        c.fill_rect(Rect::new(px + i, py + i, t, t), g);
        c.fill_rect(Rect::new(px + i, py + 2 * (s / 7) - i, t, t), g);
    }
    c.fill_rect(Rect::new(x + s / 2, py + 2 * (s / 7), s / 4, t), g);
}

/// A gear centred at (cx, cy) with outer radius `r`.
pub fn gear(c: &mut Canvas, cx: i32, cy: i32, r: i32, color: Color, hole: Color) {
    const DIRS: [(i32, i32); 8] = [(1000, 0), (707, 707), (0, 1000), (-707, 707), (-1000, 0), (-707, -707), (0, -1000), (707, -707)];
    let body = r * 3 / 4;
    let tooth = (r / 4).max(2);
    for (dx, dy) in DIRS {
        let tx = cx + dx * (r - tooth / 2) / 1000;
        let ty = cy + dy * (r - tooth / 2) / 1000;
        c.fill_rounded_rect(Rect::new(tx - tooth, ty - tooth, tooth * 2, tooth * 2), tooth / 2, color);
    }
    c.fill_circle(cx, cy, body, color);
    c.fill_circle(cx, cy, (r / 3).max(1), hole);
}
