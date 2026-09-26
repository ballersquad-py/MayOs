//! Window showing a Linux program's framebuffer (`/dev/fb0`) and feeding
//! its input devices.

use alloc::string::String;
use alloc::sync::Arc;
use core::sync::atomic::Ordering;

use gfx::{Canvas, Rect};

use super::app::{App, AppEvent, Ctx};
use crate::proc::screen::Screen;

pub struct LinuxWindow {
    screen: Arc<Screen>,
    /// Where the picture was last drawn (client coordinates).
    view: Rect,
    size: (i32, i32),
}

impl LinuxWindow {
    pub fn new(screen: Arc<Screen>) -> LinuxWindow {
        LinuxWindow { screen, view: Rect::new(0, 0, 1, 1), size: (0, 0) }
    }

    /// Largest rectangle with the framebuffer's aspect ratio that fits,
    /// drawn 1:1 when there is room.
    fn layout(&mut self) {
        let (fw, fh) = self.screen.size();
        let (fw, fh) = (fw as i32, fh as i32);
        let (w, h) = self.size;
        let (vw, vh) = if fw <= w && fh <= h {
            (fw, fh)
        } else if w * fh < h * fw {
            (w, (w as i64 * fh as i64 / fw as i64) as i32)
        } else {
            ((h as i64 * fw as i64 / fh as i64) as i32, h)
        };
        self.view = Rect::new((w - vw) / 2, (h - vh) / 2, vw.max(1), vh.max(1));
    }

    fn to_fb(&self, x: i32, y: i32) -> (i32, i32) {
        let (fw, fh) = self.screen.size();
        let v = self.view;
        ((x - v.x) * fw as i32 / v.w, (y - v.y) * fh as i32 / v.h)
    }
}

impl App for LinuxWindow {
    fn title(&self) -> String {
        self.screen.title.clone()
    }

    fn initial_size(&self) -> (i32, i32) {
        let (w, h) = self.screen.size();
        (w as i32, h as i32)
    }

    fn min_size(&self) -> (i32, i32) {
        (160, 120)
    }

    fn render(&mut self, c: &mut Canvas, size: (i32, i32), _focused: bool) {
        self.size = size;
        self.layout();
        let v = self.view;
        let buf = self.screen.buf.lock().clone();
        // Black bars around the picture.
        c.fill_rect(Rect::new(0, 0, size.0, v.y), 0xff00_0000);
        c.fill_rect(Rect::new(0, v.y + v.h, size.0, size.1 - v.y - v.h), 0xff00_0000);
        c.fill_rect(Rect::new(0, v.y, v.x, v.h), 0xff00_0000);
        c.fill_rect(Rect::new(v.x + v.w, v.y, size.0 - v.x - v.w, v.h), 0xff00_0000);
        c.blit_pixels(buf.pixels(), buf.w as i32, buf.h as i32, v);
    }

    fn event(&mut self, ev: &AppEvent, ctx: &mut Ctx) {
        match *ev {
            AppEvent::Key(ref k) => self.screen.key(k),
            AppEvent::MouseMove { x, y, .. } => {
                let (x, y) = self.to_fb(x, y);
                self.screen.motion(x, y);
            }
            AppEvent::MouseDown { x, y, button, .. } => {
                let (x, y) = self.to_fb(x, y);
                self.screen.motion(x, y);
                self.screen.button(button, true);
            }
            AppEvent::MouseUp { button, .. } => self.screen.button(button, false),
            AppEvent::Wheel { delta, .. } => self.screen.wheel(delta),
            AppEvent::Resized { w, h } => {
                self.size = (w, h);
                ctx.redraw();
            }
            _ => {}
        }
    }

    fn tick(&mut self, ctx: &mut Ctx) {
        let alive = crate::proc::process::find(self.screen.pid).is_some_and(|p| p.has_exited().is_none());
        if !alive {
            ctx.close();
            return;
        }
        // The program draws straight into shared memory: show it every frame.
        ctx.redraw_rect(self.view);
    }

    fn request_close(&mut self, _ctx: &mut Ctx) -> bool {
        self.screen.closed.store(true, Ordering::Relaxed);
        if let Some(p) = crate::proc::process::find(self.screen.pid) {
            crate::proc::process::kill(&p);
        }
        true
    }
}
