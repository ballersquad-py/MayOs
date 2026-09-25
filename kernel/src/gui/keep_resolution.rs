//! "Keep this resolution?" with a countdown: if the new mode is unusable
//! (for example it doesn't fit the VirtualBox window) the old one comes
//! back by itself.

use alloc::format;
use alloc::string::String;

use gfx::{Canvas, Rect};

use super::app::{App, AppEvent, Command, Ctx};
use super::theme::{self, fonts};
use super::widgets::{button, ButtonStyle};
use crate::input::Key;

const TIMEOUT_MS: u64 = 15_000;

pub struct KeepResolution {
    old: (u32, u32),
    new: (u32, u32),
    started: u64,
    last_secs: u64,
    hover: Option<u8>,
    done: bool,
}

impl KeepResolution {
    pub fn new(old: (u32, u32), new: (u32, u32)) -> KeepResolution {
        KeepResolution { old, new, started: crate::time::uptime_ms(), last_secs: 99, hover: None, done: false }
    }

    fn keep_rect(&self) -> Rect {
        Rect::new(300, 96, 104, 32)
    }

    fn revert_rect(&self) -> Rect {
        Rect::new(186, 96, 104, 32)
    }

    fn keep(&mut self, ctx: &mut Ctx) {
        if !self.done {
            self.done = true;
            let mode = self.new;
            crate::settings::update(|s| s.resolution = Some(mode));
            ctx.close();
        }
    }

    fn revert(&mut self, ctx: &mut Ctx) {
        if !self.done {
            self.done = true;
            ctx.commands.push(Command::RevertResolution(self.old.0, self.old.1));
            ctx.close();
        }
    }
}

impl App for KeepResolution {
    fn title(&self) -> String {
        String::from("Display")
    }
    fn initial_size(&self) -> (i32, i32) {
        (420, 144)
    }
    fn resizable(&self) -> bool {
        false
    }

    fn render(&mut self, c: &mut Canvas, (w, h): (i32, i32), _focused: bool) {
        let f = fonts();
        c.fill_rect(Rect::new(0, 0, w, h), theme::PANEL_BG);
        c.draw_text(&f.bold, 18, 30, &format!("Keep {} \u{00d7} {}?", self.new.0, self.new.1), theme::TEXT);
        let left = TIMEOUT_MS.saturating_sub(crate::time::uptime_ms() - self.started) / 1000 + 1;
        c.draw_text(&f.ui, 18, 56, &format!("Going back to {} \u{00d7} {} in {} s.", self.old.0, self.old.1, left), theme::TEXT_DIM);
        button(c, self.revert_rect(), "Revert", ButtonStyle::Normal, self.hover == Some(0), true);
        button(c, self.keep_rect(), "Keep", ButtonStyle::Primary, self.hover == Some(1), true);
    }

    fn event(&mut self, ev: &AppEvent, ctx: &mut Ctx) {
        match ev {
            AppEvent::MouseMove { x, y, .. } => {
                let h = if self.revert_rect().contains(*x, *y) {
                    Some(0)
                } else if self.keep_rect().contains(*x, *y) {
                    Some(1)
                } else {
                    None
                };
                if h != self.hover {
                    self.hover = h;
                    ctx.redraw();
                }
            }
            AppEvent::MouseDown { x, y, .. } => {
                if self.keep_rect().contains(*x, *y) {
                    self.keep(ctx);
                } else if self.revert_rect().contains(*x, *y) {
                    self.revert(ctx);
                }
            }
            AppEvent::Key(k) if k.pressed => match k.key {
                Key::Enter => self.keep(ctx),
                Key::Escape => self.revert(ctx),
                _ => {}
            },
            _ => {}
        }
    }

    fn tick(&mut self, ctx: &mut Ctx) {
        let elapsed = crate::time::uptime_ms() - self.started;
        if elapsed >= TIMEOUT_MS {
            self.revert(ctx);
            return;
        }
        let secs = elapsed / 1000;
        if secs != self.last_secs {
            self.last_secs = secs;
            ctx.redraw();
        }
    }

    fn request_close(&mut self, ctx: &mut Ctx) -> bool {
        self.revert(ctx);
        true
    }
}
