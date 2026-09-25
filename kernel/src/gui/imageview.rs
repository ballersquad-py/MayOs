//! Image viewer: PNG, JPEG and BMP, zoom and pan, previous/next picture in
//! the folder, and "Set as Wallpaper". Decoding happens on a background
//! thread so big photos don't freeze the desktop.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;

use gfx::icons::Icon;
use gfx::{rgb, with_alpha, Canvas, Rect, Surface};

use super::app::{App, AppEvent, Ctx};
use super::theme::{self, fonts};
use super::widgets::{button, ButtonStyle};
use crate::fs;
use crate::input::Key;
use crate::sync::Spin;

const TOOLBAR_H: i32 = 44;
const BG: u32 = rgb(0x1e, 0x20, 0x26);

pub fn is_image_name(name: &str) -> bool {
    matches!(fs::extension(name).as_deref(), Some("png" | "jpg" | "jpeg" | "jfif" | "bmp"))
}

pub type Slot = Arc<Spin<Option<Result<image::Image, String>>>>;

struct Job {
    path: String,
    slot: Slot,
}

extern "C" fn decode_thread(arg: usize) {
    let job = unsafe { alloc::boxed::Box::from_raw(arg as *mut Job) };
    let result = match fs::read_file(&job.path) {
        Ok(data) => image::decode(&data).map_err(|e| e.to_string()),
        Err(e) => Err(e.to_string()),
    };
    *job.slot.lock() = Some(result);
}

/// Decode a file on a kernel thread; poll the returned slot.
pub fn decode_async(path: &str) -> Slot {
    let slot: Slot = Arc::new(Spin::new(None));
    let job = alloc::boxed::Box::new(Job { path: String::from(path), slot: slot.clone() });
    crate::proc::sched::spawn_kernel("image-decode", decode_thread, alloc::boxed::Box::into_raw(job) as usize);
    slot
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Btn {
    Prev,
    Next,
    Fit,
    Actual,
    Wallpaper,
}

pub struct ImageViewer {
    path: String,
    slot: Option<Slot>,
    image: Option<image::Image>,
    error: Option<String>,
    /// Zoom in 1/100 (0 = fit to window).
    zoom: u32,
    pan: (i32, i32),
    drag: Option<(i32, i32, i32, i32)>,
    scaled: Option<(u32, u32, Surface)>,
    size: (i32, i32),
    hover: Option<Btn>,
    message: Option<(String, u64)>,
}

impl ImageViewer {
    pub fn open(path: &str) -> ImageViewer {
        ImageViewer {
            path: String::from(path),
            slot: Some(decode_async(path)),
            image: None,
            error: None,
            zoom: 0,
            pan: (0, 0),
            drag: None,
            scaled: None,
            size: (800, 560),
            hover: None,
            message: None,
        }
    }

    fn load(&mut self, path: String) {
        self.path = path;
        self.slot = Some(decode_async(&self.path));
        self.image = None;
        self.error = None;
        self.scaled = None;
        self.zoom = 0;
        self.pan = (0, 0);
    }

    fn siblings(&self) -> Vec<String> {
        let dir = fs::parent(&self.path);
        fs::read_dir(&dir)
            .map(|v| v.into_iter().filter(|e| !e.is_dir && is_image_name(&e.name)).map(|e| fs::join(&dir, &e.name)).collect())
            .unwrap_or_default()
    }

    fn step(&mut self, delta: i32) {
        let list = self.siblings();
        if list.len() < 2 {
            return;
        }
        let i = list.iter().position(|p| p == &self.path).unwrap_or(0) as i32;
        let n = list.len() as i32;
        let next = list[((i + delta) % n + n) as usize % n as usize].clone();
        self.load(next);
    }

    fn view(&self) -> Rect {
        Rect::new(0, TOOLBAR_H, self.size.0, self.size.1 - TOOLBAR_H)
    }

    fn buttons(&self) -> [(Btn, Rect, &'static str); 5] {
        let w = self.size.0;
        [
            (Btn::Prev, Rect::new(10, 7, 34, 30), "\u{25c0}"),
            (Btn::Next, Rect::new(48, 7, 34, 30), "\u{25b6}"),
            (Btn::Fit, Rect::new(92, 7, 50, 30), "Fit"),
            (Btn::Actual, Rect::new(146, 7, 56, 30), "100%"),
            (Btn::Wallpaper, Rect::new(w - 170, 7, 158, 30), "Set as Wallpaper"),
        ]
    }

    /// Displayed size of the image at the current zoom.
    fn display_size(&self, img: &image::Image) -> (u32, u32) {
        let v = self.view().inset(12);
        if self.zoom == 0 {
            let sw = v.w as u64 * 1000 / img.width as u64;
            let sh = v.h as u64 * 1000 / img.height as u64;
            let s = sw.min(sh).min(1000);
            (((img.width as u64 * s) / 1000).max(1) as u32, ((img.height as u64 * s) / 1000).max(1) as u32)
        } else {
            ((img.width * self.zoom / 100).max(1), (img.height * self.zoom / 100).max(1))
        }
    }

    fn effective_zoom(&self) -> u32 {
        match (&self.image, self.zoom) {
            (Some(img), 0) => self.display_size(img).0 * 100 / img.width.max(1),
            (_, z) => z,
        }
    }

    fn zoom_by(&mut self, up: bool) {
        let z = self.effective_zoom().max(5);
        self.zoom = if up { (z * 5 / 4).min(800) } else { (z * 4 / 5).max(5) };
        self.scaled = None;
    }
}

impl App for ImageViewer {
    fn title(&self) -> String {
        format!("{} \u{2014} Image Viewer", fs::file_name(&self.path))
    }
    fn icon(&self) -> Icon {
        Icon::Image
    }
    fn initial_size(&self) -> (i32, i32) {
        (860, 600)
    }

    fn render(&mut self, c: &mut Canvas, (w, h): (i32, i32), _focused: bool) {
        if self.size != (w, h) {
            self.size = (w, h);
            if self.zoom == 0 {
                self.scaled = None;
            }
        }
        let f = fonts();
        c.fill_rect(Rect::new(0, 0, w, TOOLBAR_H), theme::PANEL_BG);
        c.hline(0, TOOLBAR_H - 1, w, theme::SEPARATOR);
        for (b, r, label) in self.buttons() {
            let style = if b == Btn::Wallpaper { ButtonStyle::Primary } else { ButtonStyle::Normal };
            button(c, r, label, style, self.hover == Some(b), self.image.is_some() || matches!(b, Btn::Prev | Btn::Next));
        }
        if let Some(img) = &self.image {
            let info = format!("{} \u{00d7} {} \u{00b7} {}%", img.width, img.height, self.effective_zoom());
            c.draw_text(&f.ui, 214, 27, &info, theme::TEXT_DIM);
        }

        let v = self.view();
        c.fill_rect(v, BG);
        let old = c.push_clip(v);
        if let Some(img) = &self.image {
            let (dw, dh) = self.display_size(img);
            let need = self.scaled.as_ref().map(|s| (s.0, s.1) != (dw, dh)).unwrap_or(true);
            if need {
                // Very large zooms are drawn from a capped copy.
                let (sw, sh) = if dw as u64 * dh as u64 > 16_000_000 { (dw / 2, dh / 2) } else { (dw, dh) };
                let r = img.resized(sw, sh);
                let mut surf = Surface::new(sw as i32, sh as i32, BG);
                for (d, s) in surf.data.iter_mut().zip(r.pixels.iter()) {
                    *d = image::over(*s, BG);
                }
                self.scaled = Some((dw, dh, surf));
            }
            let (_, _, surf) = self.scaled.as_ref().unwrap();
            let x = v.x + (v.w - dw as i32) / 2 + self.pan.0;
            let y = v.y + (v.h - dh as i32) / 2 + self.pan.1;
            if surf.w as u32 == dw {
                c.blit(surf, x, y);
            } else {
                c.blit_scaled(surf, Rect::new(x, y, dw as i32, dh as i32), 255, 0);
            }
        } else if let Some(e) = &self.error {
            c.draw_text_centered(&f.ui, v, &format!("Cannot open this picture: {}", e), rgb(0xff, 0xb4, 0xb4));
        } else {
            c.draw_text_centered(&f.ui, v, "Loading\u{2026}", with_alpha(0xffffff, 180));
        }
        if let Some((m, _)) = &self.message {
            let tw = f.ui.measure(m) + 32;
            let r = Rect::new((w - tw) / 2, h - 50, tw, 34);
            c.fill_rounded_rect(r, 10, with_alpha(0x000000, 190));
            c.draw_text_centered(&f.ui, r, m, rgb(255, 255, 255));
        }
        c.restore_clip(old);
    }

    fn event(&mut self, ev: &AppEvent, ctx: &mut Ctx) {
        match ev {
            AppEvent::MouseMove { x, y, buttons } => {
                if let Some((sx, sy, px, py)) = self.drag
                    && buttons & 1 != 0
                {
                    self.pan = (px + x - sx, py + y - sy);
                    ctx.redraw();
                    return;
                }
                let h = self.buttons().iter().find(|(_, r, _)| r.contains(*x, *y)).map(|(b, _, _)| *b);
                if h != self.hover {
                    self.hover = h;
                    ctx.redraw();
                }
            }
            AppEvent::MouseDown { x, y, button: 0, clicks } => {
                if let Some((b, _, _)) = self.buttons().iter().find(|(_, r, _)| r.contains(*x, *y)).copied() {
                    match b {
                        Btn::Prev => self.step(-1),
                        Btn::Next => self.step(1),
                        Btn::Fit => {
                            self.zoom = 0;
                            self.pan = (0, 0);
                        }
                        Btn::Actual => {
                            self.zoom = 100;
                            self.pan = (0, 0);
                        }
                        Btn::Wallpaper => {
                            let p = self.path.clone();
                            crate::settings::update(|s| s.wallpaper_image = p);
                            self.message = Some((String::from("Wallpaper changed"), crate::time::uptime_ms() + 2500));
                        }
                    }
                } else if self.view().contains(*x, *y) {
                    if *clicks >= 2 {
                        self.zoom = if self.zoom == 0 { 100 } else { 0 };
                        self.pan = (0, 0);
                    } else {
                        self.drag = Some((*x, *y, self.pan.0, self.pan.1));
                    }
                }
                ctx.redraw();
            }
            AppEvent::MouseUp { .. } => self.drag = None,
            AppEvent::Wheel { delta, .. } => {
                self.zoom_by(*delta < 0);
                ctx.redraw();
            }
            AppEvent::Key(k) if k.pressed => {
                match k.key {
                    Key::Left | Key::PageUp => self.step(-1),
                    Key::Right | Key::PageDown | Key::Char(' ') => self.step(1),
                    Key::Char('+') | Key::Char('=') => self.zoom_by(true),
                    Key::Char('-') => self.zoom_by(false),
                    Key::Char('0') => {
                        self.zoom = 0;
                        self.pan = (0, 0);
                    }
                    Key::Char('1') => self.zoom = 100,
                    Key::Escape => ctx.close(),
                    _ => return,
                }
                ctx.redraw();
            }
            AppEvent::Resized { .. } | AppEvent::Focus(_) => ctx.redraw(),
            _ => {}
        }
    }

    fn tick(&mut self, ctx: &mut Ctx) {
        let done = self.slot.as_ref().and_then(|s| s.lock().take());
        if let Some(r) = done {
            self.slot = None;
            match r {
                Ok(img) => self.image = Some(img),
                Err(e) => self.error = Some(e),
            }
            ctx.redraw();
        }
        if let Some((_, until)) = &self.message
            && crate::time::uptime_ms() > *until
        {
            self.message = None;
            ctx.redraw();
        }
    }
}
