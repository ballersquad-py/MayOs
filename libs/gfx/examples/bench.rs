//! Rough timings of the compositor's hot paths (run with --release).
use gfx::*;
use std::time::Instant;

fn time<F: FnMut()>(name: &str, n: u32, mut f: F) {
    let t = Instant::now();
    for _ in 0..n {
        f();
    }
    println!("{:<34} {:>8.3} ms", name, t.elapsed().as_secs_f64() * 1000.0 / n as f64);
}

fn main() {
    let (w, h) = (1920, 1080);
    let mut screen = Surface::new(w, h, rgb(40, 40, 80));
    let bg = Surface::new(w, h, rgb(30, 30, 70));
    let win = Surface::new(900, 600, rgb(250, 250, 250));
    let r = Rect::new(300, 200, 900, 600);
    let mask = ShadowMask::new(900, 600, 12, 30, 6, 255, 120);
    time("background blit (full screen)", 50, || screen.canvas().blit(&bg, 0, 0));
    time("old per-frame shadow", 20, || screen.canvas().draw_shadow(r.offset(0, 8), 12, 26, rgba(0, 0, 0, 100)));
    time("cached shadow mask", 50, || screen.canvas().draw_shadow_mask(&mask, r, rgba(0, 0, 0, 100), 255));
    time("build shadow mask (once per size)", 5, || {
        let _ = ShadowMask::new(900, 600, 12, 30, 6, 255, 120);
    });
    time("rounded window blit", 50, || screen.canvas().blit_rounded(&win, r.x, r.y, 12));
    time("scaled + faded window (animation)", 20, || screen.canvas().blit_scaled(&win, r.inset(20), 180, 12));
}
