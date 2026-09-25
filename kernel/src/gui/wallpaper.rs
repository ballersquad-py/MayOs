//! Built-in wallpapers: gradients with soft glowing blobs.

use gfx::{rgb, rgba, Color, Rect, Surface};

pub struct Wallpaper {
    pub name: &'static str,
    pub top: Color,
    pub bottom: Color,
    /// (x, y, radius) as fractions of 1000 of the width/height, and colour.
    pub blobs: &'static [(i32, i32, i32, Color)],
}

pub const WALLPAPERS: &[Wallpaper] = &[
    Wallpaper {
        name: "Twilight",
        top: rgb(0x1b, 0x2a, 0x5e),
        bottom: rgb(0x5a, 0x2d, 0x7a),
        blobs: &[(750, 250, 500, rgba(0x6f, 0x8c, 0xff, 70)), (200, 750, 400, rgba(0xff, 0x7e, 0xb6, 55)), (500, 1000, 500, rgba(0x49, 0xc6, 0xe5, 45))],
    },
    Wallpaper {
        name: "Ocean",
        top: rgb(0x06, 0x3b, 0x5c),
        bottom: rgb(0x0b, 0x7a, 0x8f),
        blobs: &[(200, 200, 450, rgba(0x5e, 0xe7, 0xdf, 60)), (800, 800, 550, rgba(0x2d, 0x9c, 0xdb, 70))],
    },
    Wallpaper {
        name: "Sunset",
        top: rgb(0x3a, 0x1c, 0x5c),
        bottom: rgb(0xf0, 0x7a, 0x4a),
        blobs: &[(700, 900, 600, rgba(0xff, 0xc8, 0x57, 80)), (150, 300, 400, rgba(0xff, 0x5e, 0x8a, 55))],
    },
    Wallpaper {
        name: "Forest",
        top: rgb(0x0f, 0x2f, 0x24),
        bottom: rgb(0x2f, 0x6b, 0x3f),
        blobs: &[(300, 700, 500, rgba(0x9b, 0xe5, 0x6a, 55)), (850, 200, 400, rgba(0x3f, 0xc1, 0x9d, 50))],
    },
    Wallpaper {
        name: "Graphite",
        top: rgb(0x22, 0x25, 0x2b),
        bottom: rgb(0x40, 0x45, 0x4f),
        blobs: &[(700, 300, 500, rgba(0x9a, 0xa7, 0xbd, 40)), (200, 850, 450, rgba(0x6f, 0x7c, 0x91, 40))],
    },
    Wallpaper {
        name: "Daylight",
        top: rgb(0x8e, 0xc5, 0xfc),
        bottom: rgb(0xe0, 0xc3, 0xfc),
        blobs: &[(250, 250, 450, rgba(0xff, 0xff, 0xff, 90)), (800, 750, 500, rgba(0xff, 0xd6, 0xe8, 90))],
    },
];

pub fn render(index: usize, w: i32, h: i32) -> Surface {
    let wp = &WALLPAPERS[index.min(WALLPAPERS.len() - 1)];
    let mut s = Surface::new(w, h, 0);
    {
        let mut c = s.canvas();
        c.fill_gradient_v(Rect::new(0, 0, w, h), wp.top, wp.bottom);
        let unit = w.max(h);
        for &(fx, fy, fr, col) in wp.blobs {
            let (cx, cy, r) = (fx * w / 1000, fy * h / 1000, (fr * unit / 1000 * 6 / 10).max(1));
            let area = Rect::new(cx - r, cy - r, r * 2, r * 2);
            for y in area.y.max(0)..area.bottom().min(h) {
                for x in area.x.max(0)..area.right().min(w) {
                    let dx = (x - cx) as i64;
                    let dy = (y - cy) as i64;
                    let d = gfx::isqrt((dx * dx + dy * dy) as u64) as i32;
                    if d >= r {
                        continue;
                    }
                    let t = (r - d) as u32 * 255 / r as u32;
                    c.blend_pixel(x, y, col, t * t / 255);
                }
            }
        }
    }
    s
}
