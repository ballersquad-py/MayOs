//! The window manager and compositor.
//!
//! Every window owns a surface holding its decorations and client area.
//! Compositing only touches "damaged" screen rectangles: the background,
//! shadows, rounded window surfaces, top bar and dock are painted in
//! z-order inside each damaged rectangle, then that rectangle is sent to
//! the display (for virtio-gpu: a transfer + flush of just that region).
//!
//! Animations (open, close, minimise, restore, maximise) are time-based:
//! each frame computes where a window should appear and at what opacity,
//! and draws its existing surface scaled there.

use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use gfx::icons::{self, Icon};
use gfx::{fade, rgb, rgba, with_alpha, Canvas, Rect, ShadowMask, Surface};

use super::app::{App, AppEvent, AppKind, Command, Ctx, Msg, WindowId};
use super::display::{Display, CURSOR_DIM};
use super::theme::{self, fonts};
use crate::input::{InputEvent, Key, KeyEvent};
use crate::settings::{self, Settings};
use crate::time::uptime_ms;

const RESIZE_BORDER: i32 = 7;

const OPEN_MS: u64 = 230;
const CLOSE_MS: u64 = 170;
const MINIMIZE_MS: u64 = 280;
const MORPH_MS: u64 = 220;
const MENU_FADE_MS: u64 = 130;
const BOOT_FADE_MS: u64 = 700;
const BOUNCE_MS: u64 = 900;

/// Ease-out cubic on a 0..=1024 scale.
fn ease_out(p: i64) -> i64 {
    let q = 1024 - p.clamp(0, 1024);
    1024 - q * q / 1024 * q / 1024
}

/// Ease-in quadratic on a 0..=1024 scale.
fn ease_in(p: i64) -> i64 {
    let p = p.clamp(0, 1024);
    p * p / 1024
}

fn lerp(a: i32, b: i32, t: i64) -> i32 {
    a + ((b - a) as i64 * t / 1024) as i32
}

fn lerp_rect(a: Rect, b: Rect, t: i64) -> Rect {
    Rect::new(lerp(a.x, b.x, t), lerp(a.y, b.y, t), lerp(a.w, b.w, t).max(1), lerp(a.h, b.h, t).max(1))
}

/// `r` scaled about its centre by `s`/1024.
fn scale_rect(r: Rect, s: i64) -> Rect {
    let w = (r.w as i64 * s / 1024).max(1) as i32;
    let h = (r.h as i64 * s / 1024).max(1) as i32;
    Rect::new(r.x + (r.w - w) / 2, r.y + (r.h - h) / 2, w, h)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum AnimKind {
    /// Grow in, optionally from a rectangle (e.g. the dock icon).
    Open(Option<Rect>),
    Close,
    /// Shrink into the dock icon.
    Minimize(Rect),
    Restore(Rect),
    /// Move/resize from a previous rectangle (maximise and restore).
    Morph(Rect),
}

#[derive(Clone, Copy)]
struct Anim {
    kind: AnimKind,
    start: u64,
    duration: u64,
}

impl Anim {
    fn progress(&self, now: u64) -> i64 {
        ((now.saturating_sub(self.start)) as i64 * 1024 / self.duration.max(1) as i64).clamp(0, 1024)
    }
}

struct Window {
    id: WindowId,
    app: Box<dyn App>,
    rect: Rect,
    restore: Option<Rect>,
    minimized: bool,
    closing: bool,
    surface: Surface,
    needs_render: bool,
    hover_button: Option<u8>,
    anim: Option<Anim>,
    /// Screen area painted last frame (for damage while animating).
    last_paint: Rect,
    /// Drop shadow for the current size (rebuilt when the size changes).
    shadow: Option<ShadowMask>,
}

fn shadow_bounds(r: Rect) -> Rect {
    let b = theme::SHADOW_BLUR;
    Rect::new(r.x - b, r.y - b, r.w + 2 * b, r.h + 2 * b + theme::SHADOW_OFFSET)
}

impl Window {
    fn maximized(&self) -> bool {
        self.restore.is_some()
    }

    fn radius(&self) -> i32 {
        if self.maximized() { 0 } else { theme::WINDOW_RADIUS }
    }

    /// Everything this window paints at rest, including its shadow.
    fn paint_bounds(&self) -> Rect {
        if self.maximized() { self.rect } else { shadow_bounds(self.rect) }
    }

    /// Hidden from hit-testing (minimised, or animating out).
    fn gone(&self) -> bool {
        self.minimized || self.closing || matches!(self.anim.map(|a| a.kind), Some(AnimKind::Minimize(_)))
    }

    /// Where the window appears right now and its opacity.
    fn visual(&self, now: u64) -> (Rect, u32) {
        let Some(a) = self.anim else { return (self.rect, 255) };
        let p = a.progress(now);
        match a.kind {
            AnimKind::Open(None) => {
                let e = ease_out(p);
                let r = scale_rect(self.rect, 940 + 84 * e / 1024);
                (r.offset(0, (14 * (1024 - e) / 1024) as i32), (e * 255 / 1024) as u32)
            }
            AnimKind::Open(Some(from)) => {
                let e = ease_out(p);
                (lerp_rect(from, self.rect, e), ((e * 2).min(1024) * 255 / 1024) as u32)
            }
            AnimKind::Close => {
                let e = ease_in(p);
                (scale_rect(self.rect, 1024 - 80 * e / 1024), ((1024 - e) * 255 / 1024) as u32)
            }
            AnimKind::Minimize(target) => {
                let e = ease_in(p);
                (lerp_rect(self.rect, target, e), (255 - e * 200 / 1024) as u32)
            }
            AnimKind::Restore(from) => {
                let e = ease_out(p);
                (lerp_rect(from, self.rect, e), (55 + e * 200 / 1024) as u32)
            }
            AnimKind::Morph(from) => (lerp_rect(from, self.rect, ease_out(p)), 255),
        }
    }

    fn visual_bounds(&self, now: u64) -> Rect {
        if self.minimized && self.anim.is_none() {
            return Rect::default();
        }
        let (r, _) = self.visual(now);
        if self.maximized() && self.anim.is_none() { r } else { shadow_bounds(r) }
    }

    fn client_size(&self) -> (i32, i32) {
        (self.rect.w, self.rect.h - theme::TITLEBAR_H)
    }

    fn button_center(i: u8) -> (i32, i32) {
        (20 + i as i32 * 20, theme::TITLEBAR_H / 2)
    }
}

#[derive(Clone, Copy)]
enum Drag {
    Move { id: WindowId, dx: i32, dy: i32 },
    Resize { id: WindowId, right: bool, bottom: bool, left: bool, start: Rect, px: i32, py: i32 },
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum MenuItem {
    About,
    Settings,
    Explorer,
    Terminal,
    Editor,
    Restart,
    ShutDown,
}

const MENU: &[Option<(MenuItem, &str)>] = &[
    Some((MenuItem::About, "About MayOS")),
    Some((MenuItem::Settings, "Settings\u{2026}")),
    None,
    Some((MenuItem::Explorer, "New Explorer Window")),
    Some((MenuItem::Terminal, "New Terminal")),
    Some((MenuItem::Editor, "Text Editor")),
    None,
    Some((MenuItem::Restart, "Restart")),
    Some((MenuItem::ShutDown, "Shut Down")),
];

const DOCK: &[(AppKind, Icon, &str)] = &[
    (AppKind::Explorer, Icon::Explorer, "Files"),
    (AppKind::Terminal, Icon::Terminal, "Terminal"),
    (AppKind::Editor, Icon::Editor, "Text Editor"),
    (AppKind::Settings, Icon::Settings, "Settings"),
    (AppKind::About, Icon::Info, "About MayOS"),
];

pub struct Wm {
    display: Display,
    width: i32,
    height: i32,
    background: Surface,
    windows: Vec<Window>,
    focused: Option<WindowId>,
    next_id: WindowId,
    damage: Vec<Rect>,
    pointer: (i32, i32),
    cursor_dirty: bool,
    buttons: u8,
    drag: Option<Drag>,
    capture: Option<WindowId>,
    hovered: Option<WindowId>,
    last_click: (u64, i32, i32, u8),
    cursor: Vec<u32>,
    cursor_hot: (i32, i32),
    dock_hover: Option<usize>,
    dock_bounce: Option<(AppKind, u64)>,
    menu_open: bool,
    menu_opened_at: u64,
    menu_hover: Option<usize>,
    clock: String,
    cascade: i32,
    boot_at: u64,
    cfg: Settings,
    cfg_gen: u64,
    /// Remaining sub-pixel motion for pointer-speed scaling.
    motion_rem: (i32, i32),
    dock_shadow: Option<ShadowMask>,
    last_clock_check: u64,
    pub launcher: fn(AppKind) -> Option<Box<dyn App>>,
}

fn weekday(y: u16, m: u8, d: u8) -> &'static str {
    // Sakamoto's algorithm.
    const T: [i32; 12] = [0, 3, 2, 5, 0, 3, 5, 1, 4, 6, 2, 4];
    let mut y = y as i32;
    if m < 3 {
        y -= 1;
    }
    let w = (y + y / 4 - y / 100 + y / 400 + T[(m as usize).clamp(1, 12) - 1] + d as i32) % 7;
    ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"][w as usize]
}

fn month_name(m: u8) -> &'static str {
    ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"][(m as usize).clamp(1, 12) - 1]
}

/// Format the top-bar clock according to the settings.
pub fn format_clock(cfg: &Settings) -> String {
    let t = settings::local_time();
    let (h, suffix) = if cfg.clock_24h {
        (t.hour, "")
    } else {
        (if t.hour % 12 == 0 { 12 } else { t.hour % 12 }, if t.hour < 12 { " AM" } else { " PM" })
    };
    let time = if cfg.show_seconds {
        format!("{:02}:{:02}:{:02}{}", h, t.minute, t.second, suffix)
    } else {
        format!("{:02}:{:02}{}", h, t.minute, suffix)
    };
    format!("{} {} {}   {}", weekday(t.year, t.month, t.day), month_name(t.month), t.day, time)
}

impl Wm {
    pub fn new(mut display: Display, launcher: fn(AppKind) -> Option<Box<dyn App>>) -> Wm {
        let (width, height) = display.size();
        let (cursor, cursor_hot) = super::cursor::arrow();
        let pointer = (width / 2, height / 2);
        display.set_cursor(&cursor, cursor_hot, pointer);
        let cfg = settings::get();
        let mut wm = Wm {
            display,
            width,
            height,
            background: super::wallpaper::render(cfg.wallpaper, width, height),
            windows: Vec::new(),
            focused: None,
            next_id: 1,
            damage: Vec::new(),
            pointer,
            cursor_dirty: false,
            buttons: 0,
            drag: None,
            capture: None,
            hovered: None,
            last_click: (0, 0, 0, 0),
            cursor,
            cursor_hot,
            dock_hover: None,
            dock_bounce: None,
            menu_open: false,
            menu_opened_at: 0,
            menu_hover: None,
            clock: String::new(),
            cascade: 0,
            boot_at: uptime_ms(),
            cfg_gen: settings::generation(),
            cfg,
            motion_rem: (0, 0),
            dock_shadow: None,
            last_clock_check: 0,
            launcher,
        };
        if !wm.cfg.animations {
            wm.boot_at = 0;
        }
        wm.update_clock();
        wm.damage_all();
        wm
    }

    fn damage_all(&mut self) {
        self.damage.push(Rect::new(0, 0, self.width, self.height));
    }

    fn damage(&mut self, r: Rect) {
        let r = r.intersect(&Rect::new(0, 0, self.width, self.height));
        if !r.is_empty() {
            self.damage.push(r);
        }
    }

    fn index_of(&self, id: WindowId) -> Option<usize> {
        self.windows.iter().position(|w| w.id == id)
    }

    fn animate(&self, kind: AnimKind, duration: u64) -> Option<Anim> {
        if self.cfg.animations { Some(Anim { kind, start: uptime_ms(), duration }) } else { None }
    }

    /// True while anything on screen is moving (the desktop loop then
    /// renders frames back to back).
    pub fn is_animating(&self) -> bool {
        let now = uptime_ms();
        self.windows.iter().any(|w| w.anim.is_some())
            || self.dock_bounce.is_some()
            || (self.menu_open && now - self.menu_opened_at < MENU_FADE_MS + 20)
            || (self.boot_at != 0 && now - self.boot_at < BOOT_FADE_MS + 20)
    }

    // -----------------------------------------------------------------
    // Window lifecycle
    // -----------------------------------------------------------------

    pub fn open(&mut self, app: Box<dyn App>, over: Option<WindowId>) -> WindowId {
        self.open_from(app, over, None)
    }

    fn open_from(&mut self, app: Box<dyn App>, over: Option<WindowId>, origin: Option<Rect>) -> WindowId {
        let (mut w, mut h) = app.initial_size();
        w = w.min(self.width - 40);
        h = h.min(self.height - theme::TOPBAR_H - 90);
        let h_total = h + theme::TITLEBAR_H;
        let (x, y) = match over.and_then(|o| self.index_of(o)) {
            Some(i) => {
                let p = self.windows[i].rect;
                (p.x + (p.w - w) / 2, p.y + (p.h - h_total) / 2)
            }
            None => {
                let step = self.cascade % 8;
                self.cascade += 1;
                ((self.width - w) / 2 - 120 + step * 32, theme::TOPBAR_H + 40 + step * 28)
            }
        };
        let x = x.clamp(8, (self.width - w - 8).max(8));
        let y = y.clamp(theme::TOPBAR_H + 8, (self.height - h_total - 8).max(theme::TOPBAR_H + 8));
        let id = self.next_id;
        self.next_id += 1;
        let rect = Rect::new(x, y, w, h_total);
        let anim = self.animate(AnimKind::Open(origin), OPEN_MS);
        self.windows.push(Window {
            id,
            app,
            rect,
            restore: None,
            minimized: false,
            closing: false,
            surface: Surface::new(w, h_total, theme::WINDOW_BG),
            needs_render: true,
            hover_button: None,
            anim,
            last_paint: shadow_bounds(rect),
            shadow: None,
        });
        self.focus(Some(id));
        self.damage(shadow_bounds(rect));
        id
    }

    pub fn open_kind(&mut self, kind: AppKind) {
        let origin = DOCK.iter().position(|d| d.0 == kind).map(|i| self.dock_icon_rect(i));
        if let Some(app) = (self.launcher)(kind) {
            self.open_from(app, None, origin);
            if self.cfg.animations {
                self.dock_bounce = Some((kind, uptime_ms()));
            }
        }
    }

    /// Start closing a window (it is removed when its animation ends).
    fn close(&mut self, id: WindowId) {
        let Some(i) = self.index_of(id) else { return };
        if self.windows[i].closing {
            return;
        }
        self.windows[i].closing = true;
        let anim = self.animate(AnimKind::Close, CLOSE_MS);
        self.windows[i].anim = anim;
        if anim.is_none() {
            self.remove_window(id);
        }
        if self.capture == Some(id) {
            self.capture = None;
        }
        if self.hovered == Some(id) {
            self.hovered = None;
        }
        if self.focused == Some(id) {
            self.focused = None;
            self.focus_next();
        }
        self.damage(self.dock_damage_rect());
    }

    fn remove_window(&mut self, id: WindowId) {
        if let Some(i) = self.index_of(id) {
            let w = self.windows.remove(i);
            self.damage(w.last_paint);
            self.damage(w.paint_bounds());
        }
    }

    fn focus_next(&mut self) {
        let next = self.windows.iter().rev().find(|w| !w.gone()).map(|w| w.id);
        self.focus(next);
    }

    fn request_close(&mut self, id: WindowId) {
        let Some(i) = self.index_of(id) else { return };
        let mut ctx = Ctx::new(id);
        let ok = self.windows[i].app.request_close(&mut ctx);
        self.apply(ctx);
        if ok {
            self.close(id);
        }
    }

    fn focus(&mut self, id: Option<WindowId>) {
        if self.focused == id {
            if let Some(i) = id.and_then(|id| self.index_of(id)) {
                self.raise(i);
            }
            return;
        }
        if let Some(old) = self.focused
            && let Some(i) = self.index_of(old)
        {
            self.deliver(i, AppEvent::Focus(false));
            self.windows[i].needs_render = true;
        }
        self.focused = id;
        if let Some(i) = id.and_then(|id| self.index_of(id)) {
            self.deliver(i, AppEvent::Focus(true));
            self.windows[i].needs_render = true;
            self.raise(i);
        }
        self.damage(Rect::new(0, 0, self.width, theme::TOPBAR_H));
    }

    fn raise(&mut self, i: usize) {
        if i + 1 != self.windows.len() {
            let w = self.windows.remove(i);
            self.damage(w.paint_bounds());
            self.windows.push(w);
        }
    }

    fn dock_target(&self, kind: AppKind) -> Rect {
        match DOCK.iter().position(|d| d.0 == kind) {
            Some(i) => self.dock_icon_rect(i),
            None => {
                let d = self.dock_rect();
                Rect::new(d.x + d.w / 2 - 24, d.y, 48, 48)
            }
        }
    }

    fn minimize(&mut self, id: WindowId) {
        let Some(i) = self.index_of(id) else { return };
        let target = self.dock_target(self.windows[i].app.kind());
        match self.animate(AnimKind::Minimize(target), MINIMIZE_MS) {
            Some(a) => self.windows[i].anim = Some(a),
            None => self.windows[i].minimized = true,
        }
        let b = self.windows[i].paint_bounds();
        self.damage(b);
        if self.focused == Some(id) {
            self.focused = None;
            self.focus_next();
        }
    }

    fn unminimize(&mut self, i: usize) {
        if !self.windows[i].minimized {
            return;
        }
        let from = self.dock_target(self.windows[i].app.kind());
        self.windows[i].minimized = false;
        self.windows[i].anim = self.animate(AnimKind::Restore(from), MINIMIZE_MS);
        let b = self.windows[i].paint_bounds();
        self.damage(b);
    }

    fn toggle_maximize(&mut self, id: WindowId) {
        let Some(i) = self.index_of(id) else { return };
        if !self.windows[i].app.resizable() {
            return;
        }
        let old_rect = self.windows[i].rect;
        let old = self.windows[i].paint_bounds();
        let new = match self.windows[i].restore.take() {
            Some(r) => r,
            None => {
                self.windows[i].restore = Some(self.windows[i].rect);
                Rect::new(0, theme::TOPBAR_H, self.width, self.height - theme::TOPBAR_H)
            }
        };
        self.set_rect(i, new);
        self.windows[i].anim = self.animate(AnimKind::Morph(old_rect), MORPH_MS);
        self.damage(old);
    }

    fn set_rect(&mut self, i: usize, r: Rect) {
        let old = self.windows[i].paint_bounds();
        let w = &mut self.windows[i];
        let resized = r.w != w.rect.w || r.h != w.rect.h;
        w.rect = r;
        if resized {
            w.surface = Surface::new(r.w, r.h, theme::WINDOW_BG);
            w.needs_render = true;
            let (cw, ch) = w.client_size();
            self.deliver(i, AppEvent::Resized { w: cw, h: ch });
        }
        self.damage(old);
        let b = self.windows[i].paint_bounds();
        self.damage(b);
    }

    /// Send an event to the app in window `i` and act on its requests.
    fn deliver(&mut self, i: usize, ev: AppEvent) {
        let id = self.windows[i].id;
        let mut ctx = Ctx::new(id);
        self.windows[i].app.event(&ev, &mut ctx);
        self.apply(ctx);
    }

    fn apply(&mut self, ctx: Ctx) {
        let id = ctx.window;
        if ctx.redraw
            && let Some(i) = self.index_of(id)
        {
            self.windows[i].needs_render = true;
        }
        for cmd in ctx.commands {
            match cmd {
                Command::Open(app) => {
                    self.open(app, None);
                }
                Command::OpenChild(app) => {
                    self.open(app, Some(id));
                }
                Command::Close => self.close(id),
                Command::Send(to, msg) => self.send(to, msg),
                Command::SetResolution(w, h) => {
                    let ok = self.set_resolution(w, h);
                    if ok {
                        let mode = (self.width as u32, self.height as u32);
                        settings::update(|s| s.resolution = Some(mode));
                    }
                    self.send(id, Msg::ResolutionResult(ok));
                }
                Command::Shutdown => crate::power::shutdown(),
                Command::Reboot => crate::power::reboot(),
            }
        }
    }

    fn send(&mut self, to: WindowId, msg: Msg) {
        if let Some(i) = self.index_of(to) {
            self.deliver(i, AppEvent::Message(msg));
        }
    }

    /// Change the display mode and re-lay out the desktop.
    pub fn set_resolution(&mut self, w: u32, h: u32) -> bool {
        if (w as i32, h as i32) == (self.width, self.height) {
            return true;
        }
        if !self.display.set_mode(w, h) {
            return false;
        }
        let (width, height) = self.display.size();
        self.width = width;
        self.height = height;
        super::update_display_description(&self.display);
        self.background = super::wallpaper::render(self.cfg.wallpaper, width, height);
        let full = Rect::new(0, theme::TOPBAR_H, width, height - theme::TOPBAR_H);
        for i in 0..self.windows.len() {
            let r = self.windows[i].rect;
            let new = if self.windows[i].maximized() {
                full
            } else {
                let rw = r.w.min(width - 16);
                let rh = r.h.min(height - theme::TOPBAR_H - 16);
                Rect::new(r.x.clamp(8, (width - rw - 8).max(8)), r.y.clamp(theme::TOPBAR_H, (height - rh).max(theme::TOPBAR_H)), rw, rh)
            };
            self.set_rect(i, new);
            self.windows[i].last_paint = self.windows[i].paint_bounds();
        }
        self.pointer = (self.pointer.0.min(width - 1), self.pointer.1.min(height - 1));
        let (cursor, hot) = (core::mem::take(&mut self.cursor), self.cursor_hot);
        self.display.set_cursor(&cursor, hot, self.pointer);
        self.cursor = cursor;
        self.damage.clear();
        self.damage_all();
        true
    }

    // -----------------------------------------------------------------
    // Geometry of shell elements
    // -----------------------------------------------------------------

    fn dock_rect(&self) -> Rect {
        let n = DOCK.len() as i32;
        let w = n * theme::DOCK_ICON + (n + 1) * theme::DOCK_PAD + 8;
        let h = theme::DOCK_ICON + theme::DOCK_PAD * 2;
        Rect::new((self.width - w) / 2, self.height - h - 10, w, h)
    }

    fn dock_icon_rect(&self, i: usize) -> Rect {
        let d = self.dock_rect();
        Rect::new(
            d.x + 4 + theme::DOCK_PAD + i as i32 * (theme::DOCK_ICON + theme::DOCK_PAD),
            d.y + theme::DOCK_PAD,
            theme::DOCK_ICON,
            theme::DOCK_ICON,
        )
    }

    fn dock_damage_rect(&self) -> Rect {
        let d = self.dock_rect();
        Rect::new(d.x - 60, d.y - 60, d.w + 120, d.h + 70)
    }

    fn menu_rect(&self) -> Rect {
        let h: i32 = MENU.iter().map(|m| if m.is_some() { 28 } else { 9 }).sum();
        Rect::new(6, theme::TOPBAR_H + 4, 220, h + 12)
    }

    fn menu_item_at(&self, x: i32, y: i32) -> Option<usize> {
        let r = self.menu_rect();
        if !r.contains(x, y) {
            return None;
        }
        let mut yy = r.y + 6;
        for (i, m) in MENU.iter().enumerate() {
            let h = if m.is_some() { 28 } else { 9 };
            if y >= yy && y < yy + h {
                return if m.is_some() { Some(i) } else { None };
            }
            yy += h;
        }
        None
    }

    fn logo_rect(&self) -> Rect {
        Rect::new(4, 2, 84, theme::TOPBAR_H - 4)
    }

    fn update_clock(&mut self) -> bool {
        let s = format_clock(&self.cfg);
        if s != self.clock {
            self.clock = s;
            return true;
        }
        false
    }

    // -----------------------------------------------------------------
    // Input
    // -----------------------------------------------------------------

    pub fn handle(&mut self, ev: InputEvent) {
        match ev {
            InputEvent::MouseMove { dx, dy } => {
                // Pointer speed (1..=10, 5 = 1:1) plus acceleration.
                let speed = self.cfg.pointer_speed as i32;
                let accel = |d: i32| if d.abs() > 6 { d * 2 } else { d };
                let sx = accel(dx) * speed + self.motion_rem.0;
                let sy = accel(dy) * speed + self.motion_rem.1;
                self.motion_rem = (sx % 5, sy % 5);
                let (x, y) = (self.pointer.0 + sx / 5, self.pointer.1 + sy / 5);
                self.pointer_moved(x, y);
            }
            InputEvent::MouseAbsolute { x, y } => {
                let nx = x.map(|v| (v as i64 * self.width as i64 / 65536) as i32).unwrap_or(self.pointer.0);
                let ny = y.map(|v| (v as i64 * self.height as i64 / 65536) as i32).unwrap_or(self.pointer.1);
                self.pointer_moved(nx, ny);
            }
            InputEvent::MouseButton { button, pressed } => {
                if pressed {
                    self.buttons |= 1 << button;
                    self.mouse_down(button);
                } else {
                    self.buttons &= !(1 << button);
                    self.mouse_up(button);
                }
            }
            InputEvent::Wheel(d) => self.wheel(if self.cfg.natural_scroll { -d } else { d }),
            InputEvent::Key(k) => self.key(k),
        }
    }

    fn pointer_moved(&mut self, x: i32, y: i32) {
        let x = x.clamp(0, self.width - 1);
        let y = y.clamp(0, self.height - 1);
        if (x, y) == self.pointer {
            return;
        }
        if !self.display.has_hw_cursor() {
            let old = Rect::new(self.pointer.0 - self.cursor_hot.0, self.pointer.1 - self.cursor_hot.1, 24, 26);
            self.damage(old);
            self.damage(old.offset(x - self.pointer.0, y - self.pointer.1));
        }
        self.pointer = (x, y);
        self.cursor_dirty = true;

        if let Some(drag) = self.drag {
            self.continue_drag(drag, x, y);
            return;
        }
        if let Some(id) = self.capture
            && let Some(i) = self.index_of(id)
        {
            let r = self.windows[i].rect;
            let ev = AppEvent::MouseMove { x: x - r.x, y: y - r.y - theme::TITLEBAR_H, buttons: self.buttons };
            self.deliver(i, ev);
            return;
        }
        if self.menu_open {
            let h = self.menu_item_at(x, y);
            if h != self.menu_hover {
                self.menu_hover = h;
                self.damage(self.menu_rect());
            }
        }
        let dh = (0..DOCK.len()).find(|&i| self.dock_icon_rect(i).contains(x, y));
        if dh != self.dock_hover {
            self.dock_hover = dh;
            self.damage(self.dock_damage_rect());
        }
        let under = self.window_at(x, y);
        if under != self.hovered {
            if let Some(old) = self.hovered
                && let Some(i) = self.index_of(old)
            {
                if self.windows[i].hover_button.take().is_some() {
                    self.windows[i].needs_render = true;
                }
                self.deliver(i, AppEvent::MouseLeave);
            }
            self.hovered = under;
        }
        if let Some(i) = under.and_then(|id| self.index_of(id)) {
            let r = self.windows[i].rect;
            let (lx, ly) = (x - r.x, y - r.y);
            let hb = (0..3u8).find(|&b| {
                let (cx, cy) = Window::button_center(b);
                (lx - cx) * (lx - cx) + (ly - cy) * (ly - cy) <= 64
            });
            if hb != self.windows[i].hover_button {
                self.windows[i].hover_button = hb;
                self.windows[i].needs_render = true;
            }
            if ly >= theme::TITLEBAR_H {
                self.deliver(i, AppEvent::MouseMove { x: lx, y: ly - theme::TITLEBAR_H, buttons: self.buttons });
            }
        }
    }

    fn window_at(&self, x: i32, y: i32) -> Option<WindowId> {
        if y < theme::TOPBAR_H || self.dock_rect().contains(x, y) {
            return None;
        }
        self.windows
            .iter()
            .rev()
            .filter(|w| !w.gone())
            .find(|w| {
                // Resize handles sit outside the left, right and bottom edges.
                let r = if w.maximized() {
                    w.rect
                } else {
                    Rect::new(w.rect.x - RESIZE_BORDER, w.rect.y, w.rect.w + 2 * RESIZE_BORDER, w.rect.h + RESIZE_BORDER)
                };
                r.contains(x, y)
            })
            .map(|w| w.id)
    }

    fn continue_drag(&mut self, drag: Drag, x: i32, y: i32) {
        match drag {
            Drag::Move { id, dx, dy } => {
                if let Some(i) = self.index_of(id) {
                    let mut r = self.windows[i].rect;
                    if self.windows[i].maximized() {
                        // Dragging a maximised window restores it under the pointer.
                        let restore = self.windows[i].restore.take().unwrap();
                        r = Rect::new(x - restore.w / 2, y - 12, restore.w, restore.h);
                        self.drag = Some(Drag::Move { id, dx: x - r.x, dy: y - r.y });
                        self.set_rect(i, r);
                        return;
                    }
                    r.x = x - dx;
                    r.y = (y - dy).clamp(theme::TOPBAR_H, self.height - 40);
                    self.set_rect(i, r);
                }
            }
            Drag::Resize { id, right, bottom, left, start, px, py } => {
                if let Some(i) = self.index_of(id) {
                    let (mw, mh) = self.windows[i].app.min_size();
                    let mh = mh + theme::TITLEBAR_H;
                    let mut r = start;
                    if right {
                        r.w = (start.w + x - px).max(mw);
                    }
                    if left {
                        let w = (start.w - (x - px)).max(mw);
                        r.x = start.right() - w;
                        r.w = w;
                    }
                    if bottom {
                        r.h = (start.h + y - py).max(mh);
                    }
                    self.set_rect(i, r);
                }
            }
        }
    }

    fn is_double_click(&self, now: u64, x: i32, y: i32) -> bool {
        now - self.last_click.0 < self.cfg.double_click_ms as u64
            && (x - self.last_click.1).abs() < 5
            && (y - self.last_click.2).abs() < 5
    }

    fn mouse_down(&mut self, button: u8) {
        let (x, y) = self.pointer;

        if self.menu_open {
            if let Some(i) = self.menu_item_at(x, y) {
                if let Some((item, _)) = MENU[i] {
                    self.close_menu();
                    self.menu_action(item);
                }
                return;
            }
            self.close_menu();
            if self.logo_rect().contains(x, y) {
                return;
            }
        }

        if y < theme::TOPBAR_H {
            if self.logo_rect().contains(x, y) && button == 0 {
                self.menu_open = true;
                self.menu_opened_at = if self.cfg.animations { uptime_ms() } else { 0 };
                self.menu_hover = None;
                self.damage(self.menu_rect());
            }
            return;
        }

        if let Some(i) = (0..DOCK.len()).find(|&i| self.dock_icon_rect(i).contains(x, y)) {
            self.dock_click(DOCK[i].0, button == 1);
            return;
        }
        if self.dock_rect().contains(x, y) {
            return;
        }

        let Some(id) = self.window_at(x, y) else { return };
        self.focus(Some(id));
        let Some(i) = self.index_of(id) else { return };
        let r = self.windows[i].rect;
        let (lx, ly) = (x - r.x, y - r.y);

        if !self.windows[i].maximized() && self.windows[i].app.resizable() && button == 0 {
            let right = lx >= r.w - 2 && lx < r.w + RESIZE_BORDER;
            let bottom = ly >= r.h - 2 && ly < r.h + RESIZE_BORDER;
            let left = lx < 2 && lx >= -RESIZE_BORDER;
            if right || bottom || left {
                self.drag = Some(Drag::Resize { id, right, bottom, left, start: r, px: x, py: y });
                return;
            }
        }
        if !r.contains(x, y) {
            return;
        }

        let now = uptime_ms();
        if ly < theme::TITLEBAR_H {
            if button != 0 {
                return;
            }
            for b in 0..3u8 {
                let (cx, cy) = Window::button_center(b);
                if (lx - cx) * (lx - cx) + (ly - cy) * (ly - cy) <= 64 {
                    match b {
                        0 => self.request_close(id),
                        1 => self.minimize(id),
                        _ => self.toggle_maximize(id),
                    }
                    return;
                }
            }
            let double = self.is_double_click(now, x, y);
            self.last_click = (now, x, y, 1);
            if double {
                self.toggle_maximize(id);
                self.last_click.0 = 0;
                return;
            }
            self.drag = Some(Drag::Move { id, dx: lx, dy: ly });
            return;
        }

        let clicks = if self.is_double_click(now, x, y) { self.last_click.3.saturating_add(1) } else { 1 };
        self.last_click = (now, x, y, clicks);
        self.capture = Some(id);
        self.deliver(i, AppEvent::MouseDown { x: lx, y: ly - theme::TITLEBAR_H, button, clicks });
    }

    fn mouse_up(&mut self, button: u8) {
        let (x, y) = self.pointer;
        if self.drag.take().is_some() {
            return;
        }
        if let Some(id) = self.capture.take()
            && let Some(i) = self.index_of(id)
        {
            let r = self.windows[i].rect;
            self.deliver(i, AppEvent::MouseUp { x: x - r.x, y: y - r.y - theme::TITLEBAR_H, button });
        }
    }

    fn wheel(&mut self, delta: i32) {
        let (x, y) = self.pointer;
        if let Some(id) = self.window_at(x, y)
            && let Some(i) = self.index_of(id)
        {
            let r = self.windows[i].rect;
            self.deliver(i, AppEvent::Wheel { x: x - r.x, y: y - r.y - theme::TITLEBAR_H, delta });
        }
    }

    fn key(&mut self, k: KeyEvent) {
        if k.pressed {
            if self.menu_open && k.key == Key::Escape {
                self.close_menu();
                return;
            }
            if k.ctrl && k.alt && k.key == Key::Char('t') {
                self.open_kind(AppKind::Terminal);
                return;
            }
            if k.ctrl && k.alt && k.key == Key::Char('e') {
                self.open_kind(AppKind::Explorer);
                return;
            }
            if k.ctrl && k.alt && k.key == Key::Char('s') {
                self.dock_click(AppKind::Settings, false);
                return;
            }
            if k.alt && k.key == Key::F(4) {
                if let Some(id) = self.focused {
                    self.request_close(id);
                }
                return;
            }
            if k.alt && k.key == Key::Tab {
                if let Some(w) = self.windows.iter().find(|w| !w.gone()).map(|w| w.id) {
                    self.focus(Some(w));
                }
                return;
            }
        }
        if let Some(id) = self.focused
            && let Some(i) = self.index_of(id)
        {
            self.deliver(i, AppEvent::Key(k));
        }
    }

    fn close_menu(&mut self) {
        self.menu_open = false;
        self.damage(self.menu_rect().inset(-20));
    }

    fn menu_action(&mut self, item: MenuItem) {
        match item {
            MenuItem::About => self.dock_click(AppKind::About, false),
            MenuItem::Settings => self.dock_click(AppKind::Settings, false),
            MenuItem::Explorer => self.open_kind(AppKind::Explorer),
            MenuItem::Terminal => self.open_kind(AppKind::Terminal),
            MenuItem::Editor => self.open_kind(AppKind::Editor),
            MenuItem::Restart => crate::power::reboot(),
            MenuItem::ShutDown => crate::power::shutdown(),
        }
    }

    /// Dock click: focus (or restore) an existing window of that kind,
    /// otherwise start the app. `new_window` always opens a fresh one.
    fn dock_click(&mut self, kind: AppKind, new_window: bool) {
        if !new_window {
            let existing: Vec<WindowId> =
                self.windows.iter().filter(|w| w.app.kind() == kind && !w.closing).map(|w| w.id).collect();
            if !existing.is_empty() {
                let target = if existing.len() > 1 && self.focused == existing.last().copied() {
                    existing[0]
                } else {
                    *existing.last().unwrap()
                };
                if let Some(i) = self.index_of(target) {
                    self.unminimize(i);
                }
                self.focus(Some(target));
                return;
            }
        }
        self.open_kind(kind);
    }

    // -----------------------------------------------------------------
    // Frame
    // -----------------------------------------------------------------

    fn settings_changed(&mut self) {
        let new = settings::get();
        if new.wallpaper != self.cfg.wallpaper {
            self.background = super::wallpaper::render(new.wallpaper, self.width, self.height);
            self.damage_all();
        }
        if new.accent != self.cfg.accent {
            for w in self.windows.iter_mut() {
                w.needs_render = true;
            }
            self.damage_all();
        }
        self.cfg = new;
        self.clock.clear();
    }

    /// Tick apps, advance animations, re-render dirty windows and composite.
    pub fn frame(&mut self) {
        if settings::generation() != self.cfg_gen {
            self.cfg_gen = settings::generation();
            self.settings_changed();
        }
        for i in 0..self.windows.len() {
            if i >= self.windows.len() {
                break;
            }
            let id = self.windows[i].id;
            let mut ctx = Ctx::new(id);
            self.windows[i].app.tick(&mut ctx);
            self.apply(ctx);
        }
        let now = uptime_ms();
        // Reading the CMOS clock is slow I/O; a few times a second is plenty.
        if now - self.last_clock_check >= 250 || self.clock.is_empty() {
            self.last_clock_check = now;
            if self.update_clock() {
                self.damage(Rect::new(self.width - 300, 0, 300, theme::TOPBAR_H));
            }
        }

        // Advance animations.
        let mut finished_close = Vec::new();
        for i in 0..self.windows.len() {
            let Some(a) = self.windows[i].anim else { continue };
            let done = now >= a.start + a.duration;
            if done {
                self.windows[i].anim = None;
                match a.kind {
                    AnimKind::Close => finished_close.push(self.windows[i].id),
                    AnimKind::Minimize(_) => self.windows[i].minimized = true,
                    _ => {}
                }
            }
            let b = self.windows[i].visual_bounds(now);
            let last = self.windows[i].last_paint;
            self.damage(last.union(&b));
            self.windows[i].last_paint = if done { self.windows[i].paint_bounds() } else { b };
        }
        for id in finished_close {
            self.remove_window(id);
        }
        if let Some((_, start)) = self.dock_bounce {
            self.damage(self.dock_damage_rect());
            if now - start > BOUNCE_MS {
                self.dock_bounce = None;
            }
        }
        if self.menu_open && now - self.menu_opened_at < MENU_FADE_MS + 20 {
            self.damage(self.menu_rect().inset(-20));
        }
        if self.boot_at != 0 {
            self.damage_all();
            if now - self.boot_at > BOOT_FADE_MS {
                self.boot_at = 0;
            }
        }

        let focused = self.focused;
        for i in 0..self.windows.len() {
            if !self.windows[i].needs_render {
                continue;
            }
            let is_focused = focused == Some(self.windows[i].id);
            render_window(&mut self.windows[i], is_focused);
            if !self.windows[i].minimized {
                let b = self.windows[i].visual_bounds(now).union(&self.windows[i].paint_bounds());
                self.damage(b);
            }
        }

        // While a window is being resized its old shadow is stretched; the
        // exact one is rebuilt once the drag ends.
        let resizing = matches!(self.drag, Some(Drag::Resize { .. }));
        for w in self.windows.iter_mut() {
            let stale = w.shadow.as_ref().map(|m| (m.w, m.h) != (w.rect.w, w.rect.h)).unwrap_or(true);
            if stale && !w.maximized() && !(resizing && w.shadow.is_some()) {
                w.shadow = Some(ShadowMask::new(
                    w.rect.w,
                    w.rect.h,
                    theme::WINDOW_RADIUS,
                    theme::SHADOW_BLUR,
                    theme::SHADOW_OFFSET,
                    255,
                    120,
                ));
            }
        }
        let dock = self.dock_rect();
        if self.dock_shadow.as_ref().map(|m| (m.w, m.h) != (dock.w, dock.h)).unwrap_or(true) {
            self.dock_shadow = Some(ShadowMask::new(dock.w, dock.h, 20, 18, 4, 255, 90));
        }

        if self.cursor_dirty {
            self.cursor_dirty = false;
            self.display.move_cursor(self.pointer.0, self.pointer.1);
        }
        self.composite();
    }

    fn merged_damage(&mut self) -> Vec<Rect> {
        let mut rects: Vec<Rect> = core::mem::take(&mut self.damage);
        let mut changed = true;
        while changed {
            changed = false;
            let mut i = 0;
            while i < rects.len() {
                let mut j = i + 1;
                while j < rects.len() {
                    let a = rects[i];
                    let b = rects[j];
                    if a.inset(-16).intersects(&b) {
                        rects[i] = a.union(&b);
                        rects.swap_remove(j);
                        changed = true;
                    } else {
                        j += 1;
                    }
                }
                i += 1;
            }
        }
        if rects.len() > 6 {
            let all = rects.iter().fold(Rect::default(), |acc, r| acc.union(r));
            return alloc::vec![all];
        }
        rects
    }

    fn composite(&mut self) {
        if self.damage.is_empty() {
            return;
        }
        let rects = self.merged_damage();
        let (w, h) = (self.width, self.height);
        let screen = Rect::new(0, 0, w, h);
        let now = uptime_ms();
        for r in rects {
            let r = r.intersect(&screen);
            if r.is_empty() {
                continue;
            }
            {
                let Wm { display, background, windows, focused, .. } = self;
                let buf = display.buffer();
                let mut c = Canvas::new(buf, w, h, w as usize);
                c.push_clip(r);
                c.blit_region(background, r, r.x, r.y);
                for win in windows.iter() {
                    if win.minimized && win.anim.is_none() {
                        continue;
                    }
                    if !win.visual_bounds(now).intersects(&r) {
                        continue;
                    }
                    let shadow = if *focused == Some(win.id) { theme::SHADOW } else { theme::SHADOW_INACTIVE };
                    let (dest, alpha) = win.visual(now);
                    if dest == win.rect && alpha >= 255 {
                        let radius = win.radius();
                        if !win.maximized()
                            && let Some(m) = &win.shadow
                        {
                            if (m.w, m.h) == (win.rect.w, win.rect.h) {
                                c.draw_shadow_mask(m, win.rect, shadow, 255);
                            } else {
                                c.draw_shadow_mask_scaled(m, win.rect, shadow, 255);
                            }
                        }
                        c.blit_rounded(&win.surface, win.rect.x, win.rect.y, radius);
                        if !win.maximized() {
                            c.stroke_rounded_rect(win.rect, radius, 1, theme::BORDER);
                        }
                    } else {
                        let radius = (theme::WINDOW_RADIUS * dest.w / win.rect.w.max(1)).max(2);
                        if let Some(m) = &win.shadow {
                            c.draw_shadow_mask_scaled(m, dest, shadow, alpha);
                        }
                        c.blit_scaled(&win.surface, dest, alpha, radius);
                        c.stroke_rounded_rect(dest, radius, 1, fade(theme::BORDER, alpha));
                    }
                }
            }
            self.paint_shell(r, now);
            self.display.present(r);
        }
        self.display.end_frame();
    }

    fn bounce_offset(&self, kind: AppKind, now: u64) -> i32 {
        match self.dock_bounce {
            Some((k, start)) if k == kind => {
                let t = (now - start).min(BOUNCE_MS) as i64;
                // Two hops, the second smaller: parabolic arcs.
                let (phase, amp) = if t < 450 { (t * 1024 / 450, 18) } else { ((t - 450) * 1024 / 450, 8) };
                (amp * 4 * phase * (1024 - phase) / (1024 * 1024)) as i32
            }
            _ => 0,
        }
    }

    /// Top bar, dock, menu, boot fade and (if needed) the software cursor.
    fn paint_shell(&mut self, r: Rect, now: u64) {
        let (w, h) = (self.width, self.height);
        let focused_title = self
            .focused
            .and_then(|id| self.windows.iter().find(|x| x.id == id))
            .map(|x| x.app.title())
            .unwrap_or_default();
        let running: Vec<AppKind> = self.windows.iter().filter(|w| !w.closing).map(|w| w.app.kind()).collect();
        let dock = self.dock_rect();
        let icon_rects: Vec<Rect> = (0..DOCK.len()).map(|i| self.dock_icon_rect(i)).collect();
        let bounces: Vec<i32> = DOCK.iter().map(|d| self.bounce_offset(d.0, now)).collect();
        let menu_rect = self.menu_rect();
        let menu_alpha = if self.menu_opened_at == 0 {
            255
        } else {
            (((now - self.menu_opened_at) * 255 / MENU_FADE_MS).min(255)) as u32
        };
        let boot_alpha = if self.boot_at == 0 {
            0
        } else {
            let p = ((now - self.boot_at) as i64 * 1024 / BOOT_FADE_MS as i64).min(1024);
            (255 - ease_out(p) * 255 / 1024) as u32
        };
        let logo = self.logo_rect();
        let dock_area = self.dock_damage_rect();
        let f = fonts();
        let accent = theme::accent();
        let Wm { display, clock, dock_hover, menu_open, menu_hover, pointer, cursor, cursor_hot, dock_shadow, .. } = self;
        let hw_cursor = display.has_hw_cursor();
        let buf = display.buffer();
        let mut c = Canvas::new(buf, w, h, w as usize);
        c.push_clip(r);

        // Top bar.
        let bar = Rect::new(0, 0, w, theme::TOPBAR_H);
        if bar.intersects(&r) {
            c.fill_rect(bar, rgba(255, 255, 255, 190));
            c.hline(0, theme::TOPBAR_H - 1, w, rgba(0, 0, 0, 30));
            if *menu_open {
                c.fill_rounded_rect(logo, 6, with_alpha(0x000000, 30));
            }
            c.fill_circle(logo.x + 14, logo.y + logo.h / 2, 7, accent);
            c.fill_circle(logo.x + 14, logo.y + logo.h / 2, 3, rgb(255, 255, 255));
            let base = (theme::TOPBAR_H + f.bold.ascent - f.bold.descent) / 2;
            c.draw_text(&f.bold, logo.x + 28, base, "MayOS", theme::TEXT);
            if !focused_title.is_empty() {
                c.draw_text_clipped(&f.ui, logo.right() + 16, base, &focused_title, w / 2 - logo.right(), theme::TEXT);
            }
            let cw = f.ui.measure(clock);
            c.draw_text(&f.ui, w - cw - 16, base, clock, theme::TEXT);
            // Status indicators: network and volume.
            let mut x = w - cw - 40;
            if let Some(s) = crate::network::status() {
                let online = s.link_up && !s.ip.is_unspecified();
                let col = if online { theme::TEXT } else { with_alpha(theme::TEXT, 90) };
                for (k, bar_h) in [4, 7, 10, 13].iter().enumerate() {
                    c.fill_rounded_rect(Rect::new(x + k as i32 * 4, base - bar_h + 1, 3, *bar_h), 1, col);
                }
                x -= 26;
            }
            if crate::audio::is_present() {
                let cfg = settings::get();
                let col = theme::TEXT;
                c.fill_rect(Rect::new(x, base - 8, 3, 6), col);
                for k in 0..5 {
                    c.fill_rect(Rect::new(x + 3 + k, base - 9 - k + 1, 1, 8 + k * 2 - 2), col);
                }
                if cfg.muted || cfg.volume == 0 {
                    c.draw_text(&f.small_bold, x + 11, base, "\u{2715}", col);
                } else {
                    let arcs = if cfg.volume > 66 { 3 } else if cfg.volume > 33 { 2 } else { 1 };
                    for a in 0..arcs {
                        c.fill_rect(Rect::new(x + 10 + a * 3, base - 7 - a * 2, 1, 4 + a * 4), col);
                    }
                }
            }
        }

        // Dock.
        if dock_area.intersects(&r) {
            if let Some(m) = dock_shadow.as_ref() {
                c.draw_shadow_mask(m, dock, rgba(0, 0, 0, 70), 255);
            }
            c.fill_rounded_rect(dock, 20, rgba(255, 255, 255, 165));
            c.stroke_rounded_rect(dock, 20, 1, rgba(255, 255, 255, 200));
            for (i, (kind, icon, name)) in DOCK.iter().enumerate() {
                let ir = icon_rects[i];
                let lift = if *dock_hover == Some(i) { 4 } else { 0 } + bounces[i];
                icons::draw(&mut c, *icon, ir.x, ir.y - lift, ir.w);
                if running.contains(kind) {
                    c.fill_circle(ir.x + ir.w / 2, dock.bottom() - 5, 2, rgba(30, 30, 40, 200));
                }
                if *dock_hover == Some(i) {
                    let tw = f.ui.measure(name) + 20;
                    let tip = Rect::new(ir.x + ir.w / 2 - tw / 2, dock.y - 34, tw, 26);
                    c.fill_rounded_rect(tip, 8, rgba(30, 32, 40, 225));
                    c.draw_text_centered(&f.ui, tip, name, rgb(255, 255, 255));
                }
            }
        }

        // Menu (fades and drops in when opened).
        if *menu_open && menu_rect.inset(-20).intersects(&r) {
            let a = menu_alpha;
            let m = menu_rect.offset(0, -((255 - a as i32) * 8 / 255));
            c.draw_shadow(m, 10, 16, fade(rgba(0, 0, 0, 80), a));
            c.fill_rounded_rect(m, 10, fade(rgba(250, 251, 253, 245), a));
            c.stroke_rounded_rect(m, 10, 1, fade(rgba(0, 0, 0, 40), a));
            let mut y = m.y + 6;
            for (i, item) in MENU.iter().enumerate() {
                match item {
                    Some((_, label)) => {
                        let row = Rect::new(m.x + 6, y, m.w - 12, 28);
                        let hovered = *menu_hover == Some(i);
                        if hovered {
                            c.fill_rounded_rect(row, 6, fade(accent, a));
                        }
                        let col = if hovered { rgb(255, 255, 255) } else { theme::TEXT };
                        let base = row.y + (row.h + f.ui.ascent - f.ui.descent) / 2;
                        c.draw_text(&f.ui, row.x + 12, base, label, fade(col, a));
                        y += 28;
                    }
                    None => {
                        c.hline(m.x + 12, y + 4, m.w - 24, fade(theme::SEPARATOR, a));
                        y += 9;
                    }
                }
            }
        }

        // Boot fade-in from black.
        if boot_alpha > 0 {
            c.fill_rect(r, rgba(0, 0, 0, boot_alpha as u8));
        }

        // Software cursor when the display has no hardware cursor.
        if !hw_cursor {
            let (px, py) = (pointer.0 - cursor_hot.0, pointer.1 - cursor_hot.1);
            for yy in 0..CURSOR_DIM.min(26) {
                for xx in 0..CURSOR_DIM.min(24) {
                    let p = cursor[(yy * CURSOR_DIM + xx) as usize];
                    let a = p >> 24;
                    if a > 0 {
                        c.blend_pixel(px + xx, py + yy, p | 0xff00_0000, a);
                    }
                }
            }
        }
    }
}

fn render_window(w: &mut Window, focused: bool) {
    w.needs_render = false;
    let f = fonts();
    let title = w.app.title();
    let hover = w.hover_button;
    let (cw, ch) = w.client_size();
    let rw = w.rect.w;
    let mut c = w.surface.canvas();
    let tb = Rect::new(0, 0, rw, theme::TITLEBAR_H);
    c.fill_rect(tb, if focused { theme::TITLEBAR } else { theme::TITLEBAR_INACTIVE });
    c.hline(0, theme::TITLEBAR_H - 1, rw, theme::SEPARATOR);
    let colors = [rgb(0xff, 0x5f, 0x57), rgb(0xfe, 0xbc, 0x2e), rgb(0x28, 0xc8, 0x40)];
    for b in 0..3u8 {
        let (cx, cy) = Window::button_center(b);
        let col = if focused || hover.is_some() { colors[b as usize] } else { rgb(0xcf, 0xd0, 0xd4) };
        c.fill_circle(cx, cy, 6, col);
        if hover.is_some() {
            let g = rgba(0, 0, 0, 150);
            match b {
                0 => {
                    for d in -2..=2 {
                        c.blend_pixel(cx + d, cy + d, g, 255);
                        c.blend_pixel(cx + d, cy - d, g, 255);
                        c.blend_pixel(cx + d - 1, cy + d, g, 120);
                        c.blend_pixel(cx + d - 1, cy - d, g, 120);
                    }
                }
                1 => c.fill_rect(Rect::new(cx - 3, cy, 7, 1), g),
                _ => {
                    c.fill_rect(Rect::new(cx - 3, cy, 7, 1), g);
                    c.fill_rect(Rect::new(cx, cy - 3, 1, 7), g);
                }
            }
        }
    }
    let tcol = if focused { theme::TEXT } else { theme::TEXT_DIM };
    let tw = f.bold.measure(&title).min(rw - 170);
    let tx = (rw - tw) / 2;
    let base = (theme::TITLEBAR_H + f.bold.ascent - f.bold.descent) / 2;
    c.draw_text_clipped(&f.bold, tx.max(80), base, &title, rw - 170, tcol);

    c.translate(0, theme::TITLEBAR_H);
    let old = c.push_clip(Rect::new(0, 0, cw, ch));
    w.app.render(&mut c, (cw, ch), focused);
    c.restore_clip(old);
}
