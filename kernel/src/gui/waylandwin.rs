//! Desktop window for a Wayland toplevel (see proc/wayland.rs).

use alloc::string::String;
use alloc::sync::Arc;
use core::sync::atomic::Ordering;

use gfx::{Canvas, Rect};

use super::app::{App, AppEvent, Ctx};
use crate::proc::wayland::Window;

pub struct WaylandWindow {
    win: Arc<Window>,
    shown: u32,
    size: (i32, i32),
    /// Size last sent to the program.
    configured: (i32, i32),
    focused: bool,
}

impl WaylandWindow {
    pub fn new(win: Arc<Window>) -> WaylandWindow {
        WaylandWindow { win, shown: u32::MAX, size: (0, 0), configured: (0, 0), focused: false }
    }

    fn image_size(&self) -> (i32, i32) {
        self.win.image.lock().as_ref().map(|i| (i.0, i.1)).unwrap_or((640, 480))
    }
}

impl App for WaylandWindow {
    fn title(&self) -> String {
        self.win.title.lock().clone()
    }

    fn initial_size(&self) -> (i32, i32) {
        self.image_size()
    }

    fn min_size(&self) -> (i32, i32) {
        let (w, h) = *self.win.min_size.lock();
        (w.max(120), h.max(80))
    }

    fn render(&mut self, c: &mut Canvas, size: (i32, i32), _focused: bool) {
        self.size = size;
        let img = self.win.image.lock().clone();
        match img {
            Some((w, h, px)) => {
                c.blit_pixels(&px, w, h, Rect::new(0, 0, w, h));
                if w < size.0 {
                    c.fill_rect(Rect::new(w, 0, size.0 - w, size.1), 0xff20_2124);
                }
                if h < size.1 {
                    c.fill_rect(Rect::new(0, h, w.min(size.0), size.1 - h), 0xff20_2124);
                }
            }
            None => c.fill_rect(Rect::new(0, 0, size.0, size.1), 0xff20_2124),
        }
    }

    fn event(&mut self, ev: &AppEvent, ctx: &mut Ctx) {
        match *ev {
            AppEvent::MouseMove { x, y, .. } => self.win.pointer_motion(x, y),
            AppEvent::MouseDown { x, y, button, .. } => {
                self.win.pointer_motion(x, y);
                self.win.pointer_button(button, true);
            }
            AppEvent::MouseUp { button, .. } => self.win.pointer_button(button, false),
            AppEvent::MouseLeave => self.win.pointer_leave(),
            AppEvent::Wheel { delta, .. } => self.win.pointer_axis(delta),
            AppEvent::Key(ref k) => self.win.key(k),
            AppEvent::Focus(on) => {
                self.focused = on;
                self.win.focus(on);
            }
            AppEvent::Resized { w, h } => {
                self.size = (w, h);
                if (w, h) != self.configured {
                    self.configured = (w, h);
                    self.win.resize(w, h, self.focused);
                }
                ctx.redraw();
            }
            _ => {}
        }
    }

    fn tick(&mut self, ctx: &mut Ctx) {
        if self.win.gone.load(Ordering::Relaxed) {
            ctx.close();
            return;
        }
        // The window may have been made smaller than the program's first
        // picture (screen size): tell it the real size once.
        if self.configured == (0, 0) && self.size != (0, 0) && self.size != self.image_size() {
            self.configured = self.size;
            self.win.resize(self.size.0, self.size.1, self.focused);
        }
        let v = self.win.version.load(Ordering::Relaxed);
        if v != self.shown {
            self.shown = v;
            ctx.redraw_rect(Rect::new(0, 0, self.size.0.max(1), self.size.1.max(1)));
        }
    }

    fn request_close(&mut self, _ctx: &mut Ctx) -> bool {
        // Ask the program; the window goes when it destroys the toplevel.
        self.win.close();
        let alive = crate::proc::process::find(self.win.pid).is_some_and(|p| p.has_exited().is_none());
        !alive
    }
}
