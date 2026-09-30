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
use alloc::sync::Arc;
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
const DOCK_SLIDE_MS: u64 = 220;

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
    /// When set, only this part of the client area changed (client coords).
    render_area: Option<Rect>,
    hover_button: Option<u8>,
    anim: Option<Anim>,
    /// Screen area painted last frame (for damage while animating).
    last_paint: Rect,
    /// Drop shadow for the current size (rebuilt when the size changes).
    shadow: Option<ShadowMask>,
    /// Covering the whole screen without decorations (rect to restore).
    fullscreen: Option<Rect>,
}

fn shadow_bounds(r: Rect) -> Rect {
    let b = theme::SHADOW_BLUR;
    Rect::new(r.x - b, r.y - b, r.w + 2 * b, r.h + 2 * b + theme::SHADOW_OFFSET)
}

impl Window {
    fn maximized(&self) -> bool {
        self.restore.is_some()
    }

    /// Maximised or fullscreen: square corners, no shadow.
    fn flat(&self) -> bool {
        self.restore.is_some() || self.fullscreen.is_some()
    }

    /// Height of the title bar (none in fullscreen).
    fn title_h(&self) -> i32 {
        if self.fullscreen.is_some() { 0 } else { theme::TITLEBAR_H }
    }

    fn radius(&self) -> i32 {
        if self.flat() { 0 } else { theme::WINDOW_RADIUS }
    }

    /// Everything this window paints at rest, including its shadow.
    fn paint_bounds(&self) -> Rect {
        if self.flat() { self.rect } else { shadow_bounds(self.rect) }
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
        if self.flat() && self.anim.is_none() { r } else { shadow_bounds(r) }
    }

    fn client_size(&self) -> (i32, i32) {
        (self.rect.w, self.rect.h - self.title_h())
    }

    /// Title-bar buttons, right-aligned: 0 close, 1 minimise, 2 maximise.
    fn button_center(i: u8, w: i32) -> (i32, i32) {
        let slot = match i {
            0 => 0,
            2 => 1,
            _ => 2,
        };
        (w - 22 - slot * 34, theme::TITLEBAR_H / 2)
    }
}

#[derive(Clone, Copy)]
enum Drag {
    Move { id: WindowId, dx: i32, dy: i32 },
    Resize { id: WindowId, right: bool, bottom: bool, left: bool, start: Rect, px: i32, py: i32 },
}

/// How an app starts: a MayOS app, or a Linux program (run in a terminal
/// that shows its output) when it is installed.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Launch {
    Kind(AppKind),
    Cmd(&'static str, &'static str),
}

/// A Linux app installed with pkg (from its .desktop file).
#[derive(Clone)]
struct DesktopApp {
    name: String,
    exec: String,
    icon: String,
    terminal: bool,
}

/// Where the icon named in a .desktop file lives.
fn find_icon(name: &str) -> String {
    if name.starts_with('/') {
        return String::from(name);
    }
    for size in ["64x64", "48x48", "128x128", "96x96", "256x256", "32x32"] {
        let p = format!("/usr/share/icons/hicolor/{}/apps/{}.png", size, name);
        if crate::fs::exists(&p) {
            return p;
        }
    }
    let p = format!("/usr/share/pixmaps/{}.png", name);
    if crate::fs::exists(&p) {
        return p;
    }
    String::new()
}

/// Apps in /usr/share/applications (and the user's own), sorted by name.
fn scan_desktop_apps() -> Vec<DesktopApp> {
    let mut out: Vec<DesktopApp> = Vec::new();
    for dir in ["/usr/share/applications", "/usr/local/share/applications", "/home/.local/share/applications"] {
        let Ok(list) = crate::fs::read_dir(dir) else { continue };
        for e in list {
            if e.is_dir || !e.name.ends_with(".desktop") {
                continue;
            }
            let Ok(data) = crate::fs::read_file(&crate::fs::join(dir, &e.name)) else { continue };
            let text = String::from_utf8_lossy(&data);
            let (mut name, mut exec, mut icon, mut hidden, mut terminal, mut section) = (String::new(), String::new(), String::new(), false, false, false);
            for line in text.lines() {
                let line = line.trim();
                if line.starts_with('[') {
                    section = line == "[Desktop Entry]";
                    continue;
                }
                if !section {
                    continue;
                }
                let Some((k, v)) = line.split_once('=') else { continue };
                match k.trim() {
                    "Name" => name = String::from(v.trim()),
                    "Exec" => exec = v.split_whitespace().filter(|a| !a.starts_with('%')).collect::<Vec<_>>().join(" "),
                    "Icon" => icon = String::from(v.trim()),
                    "NoDisplay" | "Hidden" => hidden |= v.trim() == "true",
                    "Terminal" => terminal = v.trim() == "true",
                    "Type" => hidden |= v.trim() != "Application",
                    _ => {}
                }
            }
            let exec = String::from(exec.trim_matches('"'));
            if hidden || name.is_empty() || exec.is_empty() {
                continue;
            }
            // Built-in entries (Firefox, ...) already have their own.
            if APPS.iter().any(|a| a.name.eq_ignore_ascii_case(&name)) || out.iter().any(|a| a.name == name) {
                continue;
            }
            out.push(DesktopApp { name, icon: find_icon(&icon), exec, terminal });
        }
    }
    out.sort_by_key(|a| a.name.to_lowercase());
    out
}

struct AppEntry {
    name: &'static str,
    icon: Icon,
    launch: Launch,
    pinned: bool,
}

const APPS: &[AppEntry] = &[
    AppEntry { name: "Files", icon: Icon::Explorer, launch: Launch::Kind(AppKind::Explorer), pinned: true },
    AppEntry { name: "Terminal", icon: Icon::Terminal, launch: Launch::Kind(AppKind::Terminal), pinned: true },
    AppEntry { name: "Firefox", icon: Icon::Firefox, launch: Launch::Cmd("firefox", "/usr/bin/firefox"), pinned: true },
    AppEntry { name: "Minecraft", icon: Icon::Minecraft, launch: Launch::Kind(AppKind::Minecraft), pinned: true },
    AppEntry { name: "Web Browser", icon: Icon::Browser, launch: Launch::Kind(AppKind::Browser), pinned: false },
    AppEntry { name: "Text Editor", icon: Icon::Editor, launch: Launch::Kind(AppKind::Editor), pinned: false },
    AppEntry { name: "Settings", icon: Icon::Settings, launch: Launch::Kind(AppKind::Settings), pinned: true },
    AppEntry { name: "Task Manager", icon: Icon::Tasks, launch: Launch::Kind(AppKind::TaskManager), pinned: true },
    AppEntry { name: "About MayOS", icon: Icon::Info, launch: Launch::Kind(AppKind::About), pinned: false },
];

const PLACES: &[(&str, &str, Icon)] = &[
    ("Home", "/home", Icon::Home),
    ("Documents", "/docs", Icon::Documents),
    ("Downloads", "/home/Downloads", Icon::Downloads),
    ("Pictures", "/pictures", Icon::Pictures),
    ("Videos", "/videos", Icon::Video),
    ("Music", "/music", Icon::Music),
    ("Computer", "/", Icon::Computer),
];

/// Height of the bottom panel.
const PANEL_H: i32 = 46;
/// Width of the clock and status area at the right of the panel.
const TRAY_W: i32 = 190;

/// Something on the panel.
#[derive(Clone, Copy, PartialEq, Eq)]
enum PanelItem {
    Menu,
    Pin(usize),
    Win(WindowId),
}

/// Something in the app menu.
#[derive(Clone, Copy, PartialEq, Eq)]
enum MenuEntry {
    App(usize),
    Place(usize),
    Restart,
    ShutDown,
}

pub struct Wm {
    display: Display,
    width: i32,
    height: i32,
    background: Surface,
    /// Decoded picture wallpaper (path, image), kept to re-fit on resize.
    wall_image: Option<(String, Arc<image::Image>)>,
    wall_loading: Option<(String, super::imageview::Slot)>,
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
    /// Text typed into the app menu's search box.
    menu_query: String,
    /// First app shown in the menu (scrolled with the wheel).
    menu_scroll: usize,
    /// Thumbnail of the window whose taskbar button is hovered:
    /// (window, since when hovered, picture, when made).
    preview: Option<(WindowId, u64, Option<Surface>, u64)>,
    /// Apps whose program is installed (index into APPS), refreshed now and then.
    apps_ok: Vec<bool>,
    apps_checked: u64,
    /// Linux apps installed with pkg (menu entries after APPS).
    desktop_apps: Vec<DesktopApp>,
    desktop_checked: u64,
    clock: String,
    cascade: i32,
    boot_at: u64,
    cfg: Settings,
    cfg_gen: u64,
    /// Remaining sub-pixel motion for pointer-speed scaling.
    motion_rem: (i32, i32),
    /// Last absolute pointer position while the pointer is locked.
    lock_abs: (i32, i32),
    dock_shadow: Option<ShadowMask>,
    /// Dock slide animation for auto-hide: (from, to, start) offsets in px.
    dock_slide: (i32, i32, u64),
    dock_leave_at: u64,
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
            wall_image: None,
            wall_loading: None,
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
            menu_query: String::new(),
            menu_scroll: 0,
            preview: None,
            apps_ok: APPS.iter().map(|a| !matches!(a.launch, Launch::Cmd(..))).collect(),
            apps_checked: 0,
            desktop_apps: Vec::new(),
            desktop_checked: 0,
            clock: String::new(),
            cascade: 0,
            boot_at: uptime_ms(),
            cfg_gen: settings::generation(),
            cfg,
            motion_rem: (0, 0),
            lock_abs: (i32::MIN, i32::MIN),
            dock_shadow: None,
            dock_slide: (0, 0, 0),
            dock_leave_at: 0,
            last_clock_check: 0,
            launcher,
        };
        if !wm.cfg.animations {
            wm.boot_at = 0;
        }
        if wm.cfg.dock_autohide {
            let h = wm.dock_hide_distance();
            wm.dock_slide = (h, h, 0);
        }
        wm.update_clock();
        if !wm.cfg.wallpaper_image.is_empty() {
            wm.refresh_background();
        }
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
            || now.saturating_sub(self.dock_slide.2) < DOCK_SLIDE_MS + 20
    }

    // -----------------------------------------------------------------
    // Window lifecycle
    // -----------------------------------------------------------------

    pub fn open(&mut self, app: Box<dyn App>, over: Option<WindowId>) -> WindowId {
        self.open_from(app, over, None)
    }

    fn open_from(&mut self, app: Box<dyn App>, over: Option<WindowId>, origin: Option<Rect>) -> WindowId {
        let (mut w, mut h) = app.initial_size();
        let area = self.work_area();
        w = w.min(area.w - 40);
        h = h.min(area.h - theme::TITLEBAR_H - 40);
        let h_total = h + theme::TITLEBAR_H;
        let (x, y) = match over.and_then(|o| self.index_of(o)) {
            Some(i) => {
                let p = self.windows[i].rect;
                (p.x + (p.w - w) / 2, p.y + (p.h - h_total) / 2)
            }
            None => {
                let step = self.cascade % 8;
                self.cascade += 1;
                (area.x + (area.w - w) / 2 - 120 + step * 32, area.y + 30 + step * 28)
            }
        };
        let x = x.clamp(area.x + 8, (area.right() - w - 8).max(area.x + 8));
        let y = y.clamp(area.y + 8, (area.bottom() - h_total - 8).max(area.y + 8));
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
            surface: Surface::new(w, h_total, theme::window_bg()),
            needs_render: true,
            render_area: None,
            hover_button: None,
            anim,
            last_paint: shadow_bounds(rect),
            shadow: None,
            fullscreen: None,
        });
        self.focus(Some(id));
        self.damage(shadow_bounds(rect));
        id
    }

    /// Open a window in the middle of the screen.
    fn open_centered(&mut self, app: Box<dyn App>) {
        let id = self.open(app, None);
        if let Some(i) = self.index_of(id) {
            let r = self.windows[i].rect;
            let c = Rect::new((self.width - r.w) / 2, (self.height - r.h) / 3, r.w, r.h);
            self.set_rect(i, c);
            self.windows[i].last_paint = self.windows[i].paint_bounds();
        }
    }

    pub fn open_kind(&mut self, kind: AppKind) {
        let origin = self.pin_rect(kind);
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
            self.windows[i].render_area = None;
        }
        self.focused = id;
        if let Some(i) = id.and_then(|id| self.index_of(id)) {
            self.deliver(i, AppEvent::Focus(true));
            self.windows[i].needs_render = true;
            self.windows[i].render_area = None;
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
        match self.pin_rect(kind) {
            Some(r) => r,
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

    fn set_fullscreen(&mut self, id: WindowId, on: bool) {
        let Some(i) = self.index_of(id) else { return };
        if on == self.windows[i].fullscreen.is_some() {
            return;
        }
        let old = self.windows[i].paint_bounds();
        if on {
            self.windows[i].fullscreen = Some(self.windows[i].rect);
            self.set_rect(i, Rect::new(0, 0, self.width, self.height));
        } else {
            let r = self.windows[i].fullscreen.take().unwrap();
            self.set_rect(i, r);
            // The client area moved down under the title bar.
            let (cw, ch) = self.windows[i].client_size();
            self.deliver(i, AppEvent::Resized { w: cw, h: ch });
        }
        self.windows[i].needs_render = true;
        self.windows[i].render_area = None;
        self.focus(Some(id));
        self.damage(old);
        self.damage_all();
    }

    /// The topmost window if it is fullscreen (the shell is then hidden).
    fn fullscreen_top(&self) -> Option<usize> {
        let i = self.windows.iter().rposition(|w| !w.gone())?;
        if self.windows[i].fullscreen.is_some() && self.windows[i].anim.is_none() { Some(i) } else { None }
    }

    fn toggle_maximize(&mut self, id: WindowId) {
        let Some(i) = self.index_of(id) else { return };
        if !self.windows[i].app.resizable() || self.windows[i].fullscreen.is_some() {
            return;
        }
        let old_rect = self.windows[i].rect;
        let old = self.windows[i].paint_bounds();
        let new = match self.windows[i].restore.take() {
            Some(r) => r,
            None => {
                self.windows[i].restore = Some(self.windows[i].rect);
                self.work_area()
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
            w.surface = Surface::new(r.w, r.h, theme::window_bg());
            w.needs_render = true;
            w.render_area = None;
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
        if let Some(i) = self.index_of(id) {
            let w = &mut self.windows[i];
            if ctx.redraw {
                w.needs_render = true;
                w.render_area = None;
            } else if let Some(r) = ctx.redraw_area {
                if !w.needs_render {
                    w.needs_render = true;
                    w.render_area = Some(r);
                } else if let Some(a) = w.render_area {
                    w.render_area = Some(a.union(&r));
                }
            }
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
                    let old = (self.width as u32, self.height as u32);
                    let ok = self.set_resolution(w, h);
                    self.send(id, Msg::ResolutionResult(ok));
                    if ok && old != (w, h) {
                        // Centred on the screen, so it is visible even if
                        // the new mode doesn't fit the host window.
                        let keep = Box::new(super::keep_resolution::KeepResolution::new(old, (w, h)));
                        self.open_centered(keep);
                    }
                }
                Command::RevertResolution(w, h) => {
                    self.set_resolution(w, h);
                }
                Command::SetFullscreen(on) => self.set_fullscreen(id, on),
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
        self.refresh_background();
        let area = self.work_area();
        for i in 0..self.windows.len() {
            let r = self.windows[i].rect;
            let new = if self.windows[i].fullscreen.is_some() {
                Rect::new(0, 0, width, height)
            } else if self.windows[i].maximized() {
                area
            } else {
                let rw = r.w.min(area.w - 16);
                let rh = r.h.min(area.h - 16);
                Rect::new(r.x.clamp(area.x + 8, (area.right() - rw - 8).max(area.x + 8)), r.y.clamp(area.y, (area.bottom() - rh).max(area.y)), rw, rh)
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

    fn dock_icon(&self) -> i32 {
        [40, theme::DOCK_ICON, 60][self.cfg.dock_size.min(2) as usize]
    }

    fn dock_pad(&self) -> i32 {
        self.dock_icon() * theme::DOCK_PAD / theme::DOCK_ICON
    }

    fn dock_side(&self) -> u8 {
        0
    }

    /// The panel along the bottom of the screen.
    fn dock_base(&self) -> Rect {
        Rect::new(0, self.height - PANEL_H, self.width, PANEL_H)
    }

    /// How far the dock slides to be out of sight.
    fn dock_hide_distance(&self) -> i32 {
        let b = self.dock_base();
        (if self.dock_side() == 0 { b.h } else { b.w }) + 10 + 24
    }

    fn dock_offset(&self, now: u64) -> i32 {
        let (from, to, start) = self.dock_slide;
        if now >= start + DOCK_SLIDE_MS || !self.cfg.animations {
            return to;
        }
        let p = ((now - start) as i64 * 1024 / DOCK_SLIDE_MS as i64).clamp(0, 1024);
        lerp(from, to, ease_out(p))
    }

    fn dock_visible(&self) -> bool {
        self.dock_slide.1 == 0 || self.dock_offset(uptime_ms()) < self.dock_hide_distance()
    }

    fn dock_rect(&self) -> Rect {
        let b = self.dock_base();
        let o = self.dock_offset(uptime_ms());
        match self.dock_side() {
            0 => b.offset(0, o),
            1 => b.offset(-o, 0),
            _ => b.offset(o, 0),
        }
    }

    /// Pinned apps that are installed (index into APPS).
    fn pins(&self) -> Vec<usize> {
        (0..APPS.len()).filter(|&i| APPS[i].pinned && self.apps_ok.get(i).copied().unwrap_or(false)).collect()
    }

    /// Everything on the panel, left to right, with its rectangle.
    fn panel_items(&self) -> Vec<(Rect, PanelItem)> {
        let d = self.dock_rect();
        let y = d.y + 5;
        let mut out = alloc::vec![(Rect::new(6, y, 40, 36), PanelItem::Menu)];
        let mut x = 54;
        for i in self.pins() {
            out.push((Rect::new(x, y, 36, 36), PanelItem::Pin(i)));
            x += 40;
        }
        x += 12;
        let wins: Vec<WindowId> = self.windows.iter().filter(|w| !w.closing).map(|w| w.id).collect();
        if !wins.is_empty() {
            let room = self.width - TRAY_W - x - 8;
            let bw = (room / wins.len() as i32 - 4).clamp(44, 210);
            for id in wins {
                if x + bw > self.width - TRAY_W {
                    break;
                }
                out.push((Rect::new(x, y, bw, 36), PanelItem::Win(id)));
                x += bw + 4;
            }
        }
        out
    }

    /// Where a pinned app sits on the panel (window open/close animations).
    fn pin_rect(&self, kind: AppKind) -> Option<Rect> {
        self.panel_items().into_iter().find_map(|(r, it)| match it {
            PanelItem::Pin(i) if APPS[i].launch == Launch::Kind(kind) => Some(r),
            _ => None,
        })
    }

    fn launch(&mut self, i: usize) {
        if i >= APPS.len() {
            let Some(a) = self.desktop_apps.get(i - APPS.len()).cloned() else { return };
            if a.terminal {
                let t = super::terminal::Terminal::with_command("/home", &a.exec);
                self.open(Box::new(t), None);
            } else if let Err(e) = super::detached::run("/home", &a.exec) {
                let t = super::terminal::Terminal::with_command("/home", &format!("echo '{}'", e));
                self.open(Box::new(t), None);
            }
            return;
        }
        match APPS[i].launch {
            Launch::Kind(k) => self.dock_click(k, false),
            Launch::Cmd(cmd, _) => {
                // Straight to the app's window; a terminal only to show why not.
                if let Err(e) = super::detached::run("/home", cmd) {
                    let t = super::terminal::Terminal::with_command("/home", &format!("echo '{}'", e));
                    self.open(Box::new(t), None);
                }
            }
        }
    }

    /// Taskbar hover preview: after a short pause, a live thumbnail of the
    /// window (minimised ones too), refreshed a few times a second.
    fn update_preview(&mut self, now: u64) {
        let hovered = self.dock_hover.and_then(|k| self.panel_items().get(k).copied()).and_then(|(_, it)| match it {
            PanelItem::Win(id) => Some(id),
            _ => None,
        });
        let Some(id) = hovered else {
            if self.preview.take().is_some() {
                self.damage(self.dock_damage_rect().union(&self.preview_rect(0)));
            }
            return;
        };
        let since = match &self.preview {
            Some((pid, since, _, _)) if *pid == id => *since,
            _ => now,
        };
        let made = self.preview.as_ref().filter(|p| p.0 == id).map(|p| p.3).unwrap_or(0);
        let mut pic = self.preview.take().filter(|p| p.0 == id).and_then(|p| p.2);
        if now - since >= 350 && (pic.is_none() || now - made >= 400) {
            if let Some(i) = self.index_of(id) {
                let src = &self.windows[i].surface;
                let (tw, th) = (220, (220 * src.h / src.w.max(1)).clamp(60, 160));
                let mut t = Surface::new(tw, th, 0);
                {
                    let mut c = t.canvas();
                    c.blit_scaled(src, Rect::new(0, 0, tw, th), 255, 0);
                }
                pic = Some(t);
                self.preview = Some((id, since, pic, now));
                self.damage(self.preview_rect(self.item_x(id)));
                return;
            }
        }
        self.preview = Some((id, since, pic, made));
    }

    fn item_x(&self, id: WindowId) -> i32 {
        self.panel_items().iter().find(|(_, it)| *it == PanelItem::Win(id)).map(|(r, _)| r.x + r.w / 2).unwrap_or(0)
    }

    /// The preview popup above a taskbar button centred at `cx`.
    fn preview_rect(&self, cx: i32) -> Rect {
        let (w, h) = (240, 210);
        let x = (cx - w / 2).clamp(6, (self.width - w - 6).max(6));
        Rect::new(x, self.height - PANEL_H - h - 8, w, h)
    }

    fn refresh_apps(&mut self, now: u64) {
        if now - self.apps_checked < 3000 && self.apps_checked != 0 {
            return;
        }
        self.apps_checked = now;
        super::detached::poll();
        if now - self.desktop_checked >= 8000 || self.desktop_checked == 0 {
            self.desktop_checked = now;
            let found = scan_desktop_apps();
            if found.iter().map(|a| &a.name).ne(self.desktop_apps.iter().map(|a| &a.name)) {
                self.desktop_apps = found;
                if self.menu_open {
                    self.damage(self.menu_rect().inset(-20));
                }
            }
        }
        let ok: Vec<bool> = APPS
            .iter()
            .map(|a| match a.launch {
                Launch::Cmd(_, path) => crate::fs::exists(path),
                Launch::Kind(_) => true,
            })
            .collect();
        if ok != self.apps_ok {
            self.apps_ok = ok;
            self.damage(self.dock_damage_rect());
        }
    }

    fn dock_damage_rect(&self) -> Rect {
        let b = self.dock_base();
        let r = match self.dock_side() {
            0 => Rect::new(0, b.y - 50, self.width, self.height - b.y + 50),
            1 => Rect::new(0, b.y - 40, b.right() + 220, b.h + 80),
            _ => Rect::new(b.x - 220, b.y - 40, self.width - b.x + 220, b.h + 80),
        };
        r.intersect(&Rect::new(0, 0, self.width, self.height))
    }

    /// Screen area for windows: below the top bar and clear of the dock
    /// (unless it hides itself).
    pub fn work_area(&self) -> Rect {
        let full = Rect::new(0, 0, self.width, self.height);
        if self.cfg.dock_autohide {
            return full;
        }
        let b = self.dock_base();
        match self.dock_side() {
            0 => Rect::new(full.x, full.y, full.w, b.y - 6 - full.y),
            1 => Rect::new(b.right() + 6, full.y, full.w - (b.right() + 6), full.h),
            _ => Rect::new(0, full.y, b.x - 6, full.h),
        }
    }

    /// Show or hide an auto-hiding dock depending on the pointer.
    fn update_dock_visibility(&mut self, now: u64) {
        let hidden = self.dock_hide_distance();
        let want_shown = if !self.cfg.dock_autohide {
            true
        } else {
            let (x, y) = self.pointer;
            let b = self.dock_base();
            let at_edge = match self.dock_side() {
                0 => y >= self.height - 3 && x >= b.x - 40 && x <= b.right() + 40,
                1 => x <= 2 && y >= b.y - 40 && y <= b.bottom() + 40,
                _ => x >= self.width - 3 && y >= b.y - 40 && y <= b.bottom() + 40,
            };
            let over = self.dock_slide.1 == 0 && b.inset(-12).contains(x, y);
            if at_edge || over || self.drag.is_none() && self.dock_bounce.is_some() {
                self.dock_leave_at = 0;
                true
            } else if self.dock_slide.1 == 0 {
                if self.dock_leave_at == 0 {
                    self.dock_leave_at = now;
                }
                now - self.dock_leave_at < 600
            } else {
                false
            }
        };
        let target = if want_shown { 0 } else { hidden };
        if target != self.dock_slide.1 {
            let cur = self.dock_offset(now);
            self.dock_slide = (cur, target, now);
            self.damage(self.dock_damage_rect());
        }
        if now < self.dock_slide.2 + DOCK_SLIDE_MS + 20 {
            self.damage(self.dock_damage_rect());
        }
    }

    fn menu_rect(&self) -> Rect {
        let (w, h) = (580, 520.min(self.height - PANEL_H - 20));
        Rect::new(6, self.height - PANEL_H - 6 - h, w, h)
    }

    /// Apps matching the search text (index into APPS).
    fn menu_apps(&self) -> Vec<usize> {
        let q = self.menu_query.to_lowercase();
        let mut v: Vec<usize> = (0..APPS.len())
            .filter(|&i| self.apps_ok.get(i).copied().unwrap_or(false))
            .filter(|&i| q.is_empty() || APPS[i].name.to_lowercase().contains(&q))
            .collect();
        v.extend((0..self.desktop_apps.len()).filter(|&k| q.is_empty() || self.desktop_apps[k].name.to_lowercase().contains(&q)).map(|k| APPS.len() + k));
        v
    }

    /// Everything clickable in the app menu, with its rectangle.
    fn menu_entries(&self) -> Vec<(Rect, MenuEntry)> {
        let m = self.menu_rect();
        let mut out = Vec::new();
        let mut y = m.y + 94;
        for i in self.menu_apps().into_iter().skip(self.menu_scroll) {
            if y + 40 > m.bottom() - 60 {
                break;
            }
            out.push((Rect::new(m.x + 12, y, 350, 40), MenuEntry::App(i)));
            y += 42;
        }
        let px = m.x + 378;
        for (k, _) in PLACES.iter().enumerate() {
            out.push((Rect::new(px, m.y + 94 + k as i32 * 36, m.w - 390, 34), MenuEntry::Place(k)));
        }
        let by = m.bottom() - 48;
        out.push((Rect::new(px, by, (m.w - 396) / 2, 36), MenuEntry::Restart));
        out.push((Rect::new(px + (m.w - 396) / 2 + 6, by, (m.w - 396) / 2, 36), MenuEntry::ShutDown));
        out
    }

    fn menu_item_at(&self, x: i32, y: i32) -> Option<usize> {
        self.menu_entries().iter().position(|(r, _)| r.contains(x, y))
    }

    fn logo_rect(&self) -> Rect {
        Rect::new(6, self.height - PANEL_H + 5, 40, 36)
    }

    fn open_menu(&mut self) {
        self.menu_open = true;
        self.menu_opened_at = if self.cfg.animations { uptime_ms() } else { 0 };
        self.menu_hover = None;
        self.menu_query.clear();
        self.menu_scroll = 0;
        self.damage(self.menu_rect().inset(-20));
        self.damage(self.dock_damage_rect());
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
        if let Some(w) = crate::proc::wayland::pointer_lock() {
            match ev {
                InputEvent::MouseMove { dx, dy } => {
                    // A game captured the mouse: raw motion, the pointer stays put.
                    let speed = self.cfg.pointer_speed as i32;
                    w.pointer_relative(dx * speed / 5, dy * speed / 5);
                    return;
                }
                InputEvent::MouseAbsolute { x, y } => {
                    // Absolute devices (VirtualBox mouse integration): send the
                    // change; it stops at the screen edge, so turn integration
                    // off (Host+I) for games.
                    let nx = x.map(|v| (v as i64 * self.width as i64 / 65536) as i32).unwrap_or(self.lock_abs.0);
                    let ny = y.map(|v| (v as i64 * self.height as i64 / 65536) as i32).unwrap_or(self.lock_abs.1);
                    if self.lock_abs != (i32::MIN, i32::MIN) {
                        w.pointer_relative(nx - self.lock_abs.0, ny - self.lock_abs.1);
                    }
                    self.lock_abs = (nx, ny);
                    return;
                }
                _ => {}
            }
        }
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
                self.lock_abs = (i32::MIN, i32::MIN);
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
            let th = self.windows[i].title_h();
            let ev = AppEvent::MouseMove { x: x - r.x, y: y - r.y - th, buttons: self.buttons };
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
        let dh = if (self.fullscreen_top().is_some() && !self.menu_open) || !self.dock_visible() {
            None
        } else {
            self.panel_items().iter().position(|(r, _)| r.contains(x, y))
        };
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
                    self.windows[i].render_area = None;
                }
                self.deliver(i, AppEvent::MouseLeave);
            }
            self.hovered = under;
        }
        if let Some(i) = under.and_then(|id| self.index_of(id)) {
            let r = self.windows[i].rect;
            let (lx, ly) = (x - r.x, y - r.y);
            let hb = if self.windows[i].fullscreen.is_some() { None } else { (0..3u8).find(|&b| {
                let (cx, cy) = Window::button_center(b, r.w);
                (lx - cx) * (lx - cx) + (ly - cy) * (ly - cy) <= 144
            }) };
            if hb != self.windows[i].hover_button {
                self.windows[i].hover_button = hb;
                self.windows[i].needs_render = true;
                self.windows[i].render_area = None;
            }
            let th = self.windows[i].title_h();
            if ly >= th {
                self.deliver(i, AppEvent::MouseMove { x: lx, y: ly - th, buttons: self.buttons });
            }
        }
    }

    fn window_at(&self, x: i32, y: i32) -> Option<WindowId> {
        if let Some(i) = self.fullscreen_top() {
            return Some(self.windows[i].id);
        }
        if self.dock_visible() && self.dock_rect().contains(x, y) {
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
                    r.y = (y - dy).clamp(0, self.height - PANEL_H - 30);
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
        let over_shell = self.menu_open && (self.menu_rect().contains(x, y) || self.dock_rect().contains(x, y));
        if let Some(i) = self.fullscreen_top().filter(|_| !over_shell) {
            let id = self.windows[i].id;
            let now = uptime_ms();
            let clicks = if self.is_double_click(now, x, y) { self.last_click.3.saturating_add(1) } else { 1 };
            self.last_click = (now, x, y, clicks);
            self.capture = Some(id);
            self.deliver(i, AppEvent::MouseDown { x, y, button, clicks });
            return;
        }

        if self.menu_open {
            if let Some(i) = self.menu_item_at(x, y) {
                let e = self.menu_entries()[i].1;
                self.close_menu();
                self.menu_action(e);
                return;
            }
            let on_button = self.logo_rect().contains(x, y);
            if !self.menu_rect().contains(x, y) || on_button {
                self.close_menu();
            }
            if on_button || self.menu_rect().contains(x, y) {
                return;
            }
        }

        if self.dock_visible() && self.dock_rect().contains(x, y) {
            let hit = self.panel_items().into_iter().find(|(r, _)| r.contains(x, y));
            match hit {
                Some((_, PanelItem::Menu)) if button == 0 => self.open_menu(),
                Some((_, PanelItem::Pin(i))) => match APPS[i].launch {
                    Launch::Kind(k) => self.dock_click(k, button == 1),
                    _ => self.launch(i),
                },
                Some((_, PanelItem::Win(id))) => {
                    // Like a taskbar: minimise the active window, bring others up.
                    if let Some(i) = self.index_of(id) {
                        if self.focused == Some(id) && !self.windows[i].minimized {
                            self.minimize(id);
                        } else {
                            self.unminimize(i);
                            self.focus(Some(id));
                        }
                    }
                }
                _ => {}
            }
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
                let (cx, cy) = Window::button_center(b, r.w);
                if (lx - cx) * (lx - cx) + (ly - cy) * (ly - cy) <= 144 {
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
        let th = self.windows[i].title_h();
        self.deliver(i, AppEvent::MouseDown { x: lx, y: ly - th, button, clicks });
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
            let th = self.windows[i].title_h();
            self.deliver(i, AppEvent::MouseUp { x: x - r.x, y: y - r.y - th, button });
        }
    }

    fn wheel(&mut self, delta: i32) {
        let (x, y) = self.pointer;
        if self.menu_open && self.menu_rect().contains(x, y) {
            let n = self.menu_apps().len();
            let s = self.menu_scroll as i32 - delta.signum() * 2;
            self.menu_scroll = s.clamp(0, n.saturating_sub(4) as i32) as usize;
            self.damage(self.menu_rect().inset(-20));
            return;
        }
        if let Some(id) = self.window_at(x, y)
            && let Some(i) = self.index_of(id)
        {
            let r = self.windows[i].rect;
            let th = self.windows[i].title_h();
            self.deliver(i, AppEvent::Wheel { x: x - r.x, y: y - r.y - th, delta });
        }
    }

    fn key(&mut self, k: KeyEvent) {
        if k.pressed {
            if k.key == Key::Super {
                if self.menu_open {
                    self.close_menu();
                } else {
                    self.open_menu();
                }
                return;
            }
            if self.menu_open {
                match k.key {
                    Key::Escape => self.close_menu(),
                    Key::Enter => {
                        if let Some(&i) = self.menu_apps().first() {
                            self.close_menu();
                            self.launch(i);
                        }
                    }
                    Key::Backspace => {
                        self.menu_query.pop();
                        self.menu_scroll = 0;
                        self.damage(self.menu_rect());
                    }
                    Key::Char(ch) if !k.ctrl && !k.alt => {
                        self.menu_query.push(ch);
                        self.menu_scroll = 0;
                        self.damage(self.menu_rect());
                    }
                    _ => {}
                }
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
            // Task Manager: Ctrl+Shift+Esc (or Ctrl+Alt+Delete).
            if k.ctrl && ((k.shift && k.key == Key::Escape) || (k.alt && k.key == Key::Delete)) {
                self.dock_click(AppKind::TaskManager, false);
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
        self.damage(self.dock_damage_rect());
    }

    fn menu_action(&mut self, e: MenuEntry) {
        match e {
            MenuEntry::App(i) => self.launch(i),
            MenuEntry::Place(k) => {
                let path = PLACES[k].1;
                if path == "/home/Downloads" {
                    let _ = crate::fs::create_dir(path);
                }
                self.open(Box::new(super::explorer::Explorer::new(path)), None);
            }
            MenuEntry::Restart => crate::power::reboot(),
            MenuEntry::ShutDown => crate::power::shutdown(),
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
        if new.dark != self.cfg.dark || new.accent != self.cfg.accent {
            // Every window redraws in the new colours.
            for w in self.windows.iter_mut() {
                w.needs_render = true;
                w.render_area = None;
            }
            self.damage_all();
        }
        let wall_changed = new.wallpaper != self.cfg.wallpaper || new.wallpaper_image != self.cfg.wallpaper_image;
        if new.accent != self.cfg.accent {
            for w in self.windows.iter_mut() {
                w.needs_render = true;
                w.render_area = None;
            }
            self.damage_all();
        }
        let dock_changed = (new.dock_position, new.dock_autohide, new.dock_size) != (self.cfg.dock_position, self.cfg.dock_autohide, self.cfg.dock_size);
        let old_dock = self.dock_damage_rect();
        self.cfg = new;
        if wall_changed {
            self.refresh_background();
        }
        if dock_changed {
            let target = if self.cfg.dock_autohide { self.dock_hide_distance() } else { 0 };
            self.dock_slide = (target, target, 0);
            self.dock_shadow = None;
            self.damage(old_dock);
            // Re-fit maximised windows to the new work area.
            let area = self.work_area();
            for i in 0..self.windows.len() {
                if self.windows[i].maximized() && self.windows[i].fullscreen.is_none() {
                    self.set_rect(i, area);
                    self.windows[i].last_paint = self.windows[i].paint_bounds();
                }
            }
            self.damage_all();
        }
        self.clock.clear();
    }

    /// Rebuild the desktop background for the current size and settings.
    /// A picture wallpaper is decoded on a background thread the first
    /// time; until it's ready the built-in one is shown.
    fn refresh_background(&mut self) {
        let path = self.cfg.wallpaper_image.clone();
        if !path.is_empty() {
            if let Some((p, img)) = &self.wall_image {
                if *p == path {
                    let fitted = img.cover(self.width as u32, self.height as u32, 0xff00_0000);
                    let mut s = Surface::new(self.width, self.height, 0);
                    for (d, px) in s.data.iter_mut().zip(fitted.pixels.iter()) {
                        *d = *px | 0xff00_0000;
                    }
                    self.background = s;
                    self.damage_all();
                    return;
                }
            }
            if self.wall_loading.as_ref().map(|(p, _)| *p != path).unwrap_or(true) {
                self.wall_loading = Some((path.clone(), super::imageview::decode_async(&path)));
            }
        } else {
            self.wall_image = None;
            self.wall_loading = None;
        }
        self.background = super::wallpaper::render(self.cfg.wallpaper, self.width, self.height);
        self.damage_all();
    }

    fn poll_wallpaper(&mut self) {
        let Some((path, slot)) = &self.wall_loading else { return };
        let Some(result) = slot.lock().take() else { return };
        let path = path.clone();
        self.wall_loading = None;
        match result {
            Ok(img) => {
                self.wall_image = Some((path, Arc::new(img)));
                self.refresh_background();
            }
            Err(e) => crate::kprintln!("wallpaper: {}: {}", path, e),
        }
    }

    /// Tick apps, advance animations, re-render dirty windows and composite.
    pub fn frame(&mut self) {
        self.poll_wallpaper();
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
        self.refresh_apps(now);
        self.update_preview(now);
        self.update_dock_visibility(now);
        // Reading the CMOS clock is slow I/O; a few times a second is plenty.
        if now - self.last_clock_check >= 250 || self.clock.is_empty() {
            self.last_clock_check = now;
            if self.update_clock() {
                self.damage(Rect::new(self.width - TRAY_W, self.height - PANEL_H, TRAY_W, PANEL_H));
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
            // Partial update: only that part of the client is redrawn and
            // composited (e.g. a visualiser or a clock).
            let w = &self.windows[i];
            if let Some(area) = w.render_area
                && w.anim.is_none()
                && !w.minimized
            {
                let (cw, ch) = w.client_size();
                let area = area.intersect(&Rect::new(0, 0, cw, ch));
                let top = w.title_h();
                let (wx, wy) = (w.rect.x, w.rect.y);
                render_window_area(&mut self.windows[i], is_focused, area);
                if !area.is_empty() {
                    self.damage(area.offset(wx, wy + top));
                }
                continue;
            }
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
            if stale && !w.flat() && !(resizing && w.shadow.is_some()) {
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
                // Background and windows, in bands drawn by every CPU at
                // once (each band only touches its own rows).
                let Wm { display, background, windows, focused, .. } = self;
                struct Layer<'a> {
                    surface: &'a Surface,
                    shadow: Option<&'a ShadowMask>,
                    rect: Rect,
                    dest: Rect,
                    alpha: u32,
                    radius: i32,
                    flat: bool,
                    color: gfx::Color,
                }
                let layers: Vec<Layer> = windows
                    .iter()
                    .filter(|w| !(w.minimized && w.anim.is_none()) && w.visual_bounds(now).intersects(&r))
                    .map(|w| {
                        let (dest, alpha) = w.visual(now);
                        Layer {
                            surface: &w.surface,
                            shadow: w.shadow.as_ref(),
                            rect: w.rect,
                            dest,
                            alpha,
                            radius: w.radius(),
                            flat: w.flat(),
                            color: if *focused == Some(w.id) { theme::SHADOW } else { theme::SHADOW_INACTIVE },
                        }
                    })
                    .collect();
                let buf = display.buffer();
                let (ptr, len) = (buf.as_mut_ptr() as usize, buf.len());
                let background = &*background;
                let border = theme::border();
                let band_h = 48;
                let parts = (r.h as usize).div_ceil(band_h as usize).max(1);
                let draw = |i: usize| {
                    let band = Rect::new(r.x, r.y + i as i32 * band_h, r.w, band_h).intersect(&r);
                    if band.is_empty() {
                        return;
                    }
                    // SAFETY: bands are disjoint rows of the frame buffer,
                    // and the clip keeps each canvas inside its band.
                    let buf = unsafe { core::slice::from_raw_parts_mut(ptr as *mut u32, len) };
                    let mut c = Canvas::new(buf, w, h, w as usize);
                    c.push_clip(band);
                    c.blit_region(background, band, band.x, band.y);
                    for l in &layers {
                        if l.dest == l.rect && l.alpha >= 255 {
                            if !l.flat
                                && let Some(m) = l.shadow
                            {
                                if (m.w, m.h) == (l.rect.w, l.rect.h) {
                                    c.draw_shadow_mask(m, l.rect, l.color, 255);
                                } else {
                                    c.draw_shadow_mask_scaled(m, l.rect, l.color, 255);
                                }
                            }
                            c.blit_rounded(l.surface, l.rect.x, l.rect.y, l.radius);
                            if !l.flat {
                                c.stroke_rounded_rect(l.rect, l.radius, 1, border);
                            }
                        } else {
                            let radius = (theme::WINDOW_RADIUS * l.dest.w / l.rect.w.max(1)).max(2);
                            if let Some(m) = l.shadow {
                                c.draw_shadow_mask_scaled(m, l.dest, l.color, l.alpha);
                            }
                            c.blit_scaled(l.surface, l.dest, l.alpha, radius);
                            c.stroke_rounded_rect(l.dest, radius, 1, fade(border, l.alpha));
                        }
                    }
                };
                if parts > 1 && r.w * r.h > 60_000 {
                    crate::parallel::run(parts, &draw);
                } else {
                    for i in 0..parts {
                        draw(i);
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
        let covered = self.fullscreen_top().is_some();
        let dock_side = self.dock_side();
        // In fullscreen the panel hides; the menu (Super) brings it back.
        let dock_on = self.dock_visible() && (!covered || self.menu_open);
        let focused_title = self
            .focused
            .and_then(|id| self.windows.iter().find(|x| x.id == id))
            .map(|x| x.app.title())
            .unwrap_or_default();
        let running: Vec<AppKind> = self.windows.iter().filter(|w| !w.closing).map(|w| w.app.kind()).collect();
        let dock = self.dock_rect();
        let items = self.panel_items();
        let win_info: Vec<(WindowId, String, Icon, bool)> =
            self.windows.iter().filter(|w| !w.closing).map(|w| (w.id, w.app.title(), w.app.icon(), w.minimized)).collect();
        let focused_id = self.focused;
        let menu_rect = self.menu_rect();
        let menu_entries = if self.menu_open { self.menu_entries() } else { Vec::new() };
        let menu_query = self.menu_query.clone();
        let desktop_apps = if self.menu_open { self.desktop_apps.clone() } else { Vec::new() };
        let preview = match &self.preview {
            Some((id, _, Some(pic), _)) => {
                let title = self.windows.iter().find(|w| w.id == *id).map(|w| w.app.title()).unwrap_or_default();
                Some((self.preview_rect(self.item_x(*id)), pic.clone(), title))
            }
            _ => None,
        };
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
        let Wm { display, clock, dock_hover, menu_open, menu_hover, pointer, cursor, cursor_hot, .. } = self;
        let hw_cursor = display.has_hw_cursor();
        let buf = display.buffer();
        let mut c = Canvas::new(buf, w, h, w as usize);
        c.push_clip(r);
        let _ = (&focused_title, dock_side, logo);
        let white = rgb(255, 255, 255);

        // Panel (Cinnamon-style, dark glass).
        if dock_area.intersects(&r) && dock_on {
            c.fill_rect(dock, rgba(24, 26, 31, 232));
            c.hline(0, dock.y, w, rgba(255, 255, 255, 22));
            for (k, (ir, item)) in items.iter().enumerate() {
                let hovered = *dock_hover == Some(k);
                match *item {
                    PanelItem::Menu => {
                        if hovered || *menu_open {
                            c.fill_rounded_rect(*ir, 8, rgba(255, 255, 255, if *menu_open { 36 } else { 22 }));
                        }
                        super::icons::draw(&mut c, Icon::MayOS, ir.x + (ir.w - 28) / 2, ir.y + 4, 28);
                    }
                    PanelItem::Pin(i) => {
                        if hovered {
                            c.fill_rounded_rect(*ir, 8, rgba(255, 255, 255, 22));
                        }
                        super::icons::draw(&mut c, APPS[i].icon, ir.x + 4, ir.y + 4, 28);
                        if let Launch::Kind(k2) = APPS[i].launch
                            && running.contains(&k2)
                        {
                            c.fill_rounded_rect(Rect::new(ir.x + 12, ir.bottom() - 3, 12, 3), 1, accent);
                        }
                        if hovered {
                            let name = APPS[i].name;
                            let tw = f.ui.measure(name) + 20;
                            let tip = Rect::new(ir.x + ir.w / 2 - tw / 2, dock.y - 34, tw, 26);
                            c.fill_rounded_rect(tip, 8, rgba(30, 32, 40, 235));
                            c.draw_text_centered(&f.ui, tip, name, white);
                        }
                    }
                    PanelItem::Win(id) => {
                        let Some((_, title, icon, minimized)) = win_info.iter().find(|x| x.0 == id) else { continue };
                        let active = focused_id == Some(id) && !*minimized;
                        let bg = if active { 44 } else if hovered { 30 } else { 14 };
                        if active {
                            // A short accent pill under the middle, like the pinned apps'.
                            c.fill_rounded_rect(*ir, 8, rgba(255, 255, 255, bg));
                            c.fill_rounded_rect(Rect::new(ir.x + ir.w / 2 - 12, ir.bottom() - 4, 24, 3), 1, accent);
                        } else {
                            c.fill_rounded_rect(*ir, 8, rgba(255, 255, 255, bg));
                        }
                        super::icons::draw(&mut c, *icon, ir.x + 8, ir.y + 8, 20);
                        let base = ir.y + (ir.h + f.ui.ascent - f.ui.descent) / 2;
                        let col = if *minimized { rgba(255, 255, 255, 140) } else { white };
                        c.draw_text_clipped(&f.ui, ir.x + 34, base, title, ir.w - 42, col);
                    }
                }
            }
            // Window preview above its taskbar button.
            if let Some((pr, pic, title)) = &preview {
                let card = Rect::new(pr.x, pr.bottom() - pic.h - 44, pr.w, pic.h + 44);
                c.draw_shadow(card, 12, 18, rgba(0, 0, 0, 90));
                c.fill_rounded_rect(card, 12, rgba(34, 36, 42, 245));
                c.stroke_rounded_rect(card, 12, 1, rgba(255, 255, 255, 30));
                c.draw_text_clipped(&f.bold, card.x + 12, card.y + 22, title, card.w - 24, white);
                c.blit_scaled(pic, Rect::new(card.x + 10, card.y + 34, pic.w, pic.h), 255, 6);
            }
            // Tray: network and volume icons, then the time over the date.
            let (time, date) = match clock.trim().rsplit_once(' ') {
                Some((d, t)) => (t.trim(), d.trim()),
                None => (clock.as_str(), ""),
            };
            let date: String = {
                // "Wed Sep 30" -> "Wed, 30 Sep"
                let p: Vec<&str> = date.split_whitespace().collect();
                if p.len() == 3 { format!("{}, {} {}", p[0], p[2], p[1]) } else { String::from(date) }
            };
            let tray = Rect::new(w - TRAY_W, dock.y, TRAY_W, PANEL_H);
            let text_w = f.medium.measure(time).max(f.small_bold.measure(&date));
            let tx = w - 14 - text_w;
            c.draw_text(&f.medium, tx + (text_w - f.medium.measure(time)) / 2, dock.y + 22, time, white);
            c.draw_text(&f.small_bold, tx + (text_w - f.small_bold.measure(&date)) / 2, dock.y + 38, &date, rgba(255, 255, 255, 165));
            let mut x = tx - 16 - 20;
            let iy = dock.y + (PANEL_H - 20) / 2;
            if let Some(st) = crate::network::status() {
                let online = st.link_up && !st.ip.is_unspecified();
                let icon = if !online {
                    Icon::NetOff
                } else if st.adapter.contains("tethering") || st.adapter.contains("iPhone") {
                    Icon::NetPhone
                } else {
                    Icon::NetWired
                };
                super::icons::draw(&mut c, icon, x, iy, 20);
                x -= 30;
            }
            if crate::audio::is_present() {
                let cfg = settings::get();
                let icon = if cfg.muted || cfg.volume == 0 {
                    Icon::VolMute
                } else if cfg.volume > 66 {
                    Icon::VolHigh
                } else if cfg.volume > 33 {
                    Icon::VolMed
                } else {
                    Icon::VolLow
                };
                super::icons::draw(&mut c, icon, x, iy, 20);
            }
            let _ = tray;
        }

        // App menu: search, apps, places and power (fades up when opened).
        if *menu_open && menu_rect.inset(-20).intersects(&r) {
            let a = menu_alpha;
            let m = menu_rect.offset(0, (255 - a as i32) * 10 / 255);
            c.draw_shadow(m, 12, 20, fade(rgba(0, 0, 0, 110), a));
            c.fill_rounded_rect(m, 12, fade(rgba(30, 32, 38, 246), a));
            c.stroke_rounded_rect(m, 12, 1, fade(rgba(255, 255, 255, 30), a));
            // Search box.
            let sb = Rect::new(m.x + 12, m.y + 14, m.w - 24, 38);
            c.fill_rounded_rect(sb, 10, fade(rgba(255, 255, 255, 20), a));
            let sbase = sb.y + (sb.h + f.ui.ascent - f.ui.descent) / 2;
            c.fill_circle(sb.x + 20, sb.y + 17, 6, fade(rgba(255, 255, 255, 150), a));
            c.fill_circle(sb.x + 20, sb.y + 17, 4, fade(rgba(30, 32, 38, 255), a));
            c.fill_rect(Rect::new(sb.x + 24, sb.y + 21, 2, 6), fade(rgba(255, 255, 255, 150), a));
            if menu_query.is_empty() {
                c.draw_text(&f.ui, sb.x + 38, sbase, "Type to search apps\u{2026}", fade(rgba(255, 255, 255, 110), a));
            } else {
                let end = c.draw_text(&f.ui, sb.x + 38, sbase, &menu_query, fade(white, a));
                c.fill_rect(Rect::new(end + 1, sb.y + 10, 2, sb.h - 20), fade(accent, a));
            }
            c.draw_text(&f.small_bold, m.x + 20, m.y + 82, "APPLICATIONS", fade(rgba(255, 255, 255, 120), a));
            c.draw_text(&f.small_bold, m.x + 386, m.y + 82, "PLACES", fade(rgba(255, 255, 255, 120), a));
            c.fill_rect(Rect::new(m.x + 368, m.y + 70, 1, m.h - 84), fade(rgba(255, 255, 255, 20), a));
            for (k, (er, e)) in menu_entries.iter().enumerate() {
                let hovered = *menu_hover == Some(k);
                let base = er.y + (er.h + f.ui.ascent - f.ui.descent) / 2;
                match *e {
                    MenuEntry::App(i) => {
                        if hovered {
                            c.fill_rounded_rect(*er, 8, fade(accent, a));
                        }
                        if let Some(d) = i.checked_sub(APPS.len()).and_then(|k| desktop_apps.get(k)) {
                            super::icons::draw_file(&mut c, &d.icon, Icon::Program, er.x + 8, er.y + 4, 32);
                            c.draw_text(&f.ui, er.x + 50, base, &d.name, fade(white, a));
                        } else {
                            super::icons::draw(&mut c, APPS[i].icon, er.x + 8, er.y + 4, 32);
                            c.draw_text(&f.ui, er.x + 50, base, APPS[i].name, fade(white, a));
                        }
                    }
                    MenuEntry::Place(p) => {
                        if hovered {
                            c.fill_rounded_rect(*er, 8, fade(rgba(255, 255, 255, 26), a));
                        }
                        super::icons::draw(&mut c, PLACES[p].2, er.x + 6, er.y + 5, 24);
                        c.draw_text(&f.ui, er.x + 38, base, PLACES[p].0, fade(rgba(255, 255, 255, 220), a));
                    }
                    MenuEntry::Restart | MenuEntry::ShutDown => {
                        let danger = *e == MenuEntry::ShutDown;
                        let bg = if hovered { if danger { theme::DANGER } else { rgba(255, 255, 255, 40) } } else { rgba(255, 255, 255, 18) };
                        c.fill_rounded_rect(*er, 8, fade(bg, a));
                        let label = if danger { "Shut down" } else { "Restart" };
                        c.draw_text_centered(&f.ui, *er, label, fade(white, a));
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

/// Re-render only `area` of the client (client coordinates); the rest of
/// the window surface keeps its pixels.
fn render_window_area(w: &mut Window, focused: bool, area: Rect) {
    w.needs_render = false;
    w.render_area = None;
    if area.is_empty() {
        return;
    }
    let (cw, ch) = w.client_size();
    let top = w.title_h();
    let mut c = w.surface.canvas();
    c.translate(0, top);
    let old = c.push_clip(area);
    w.app.render(&mut c, (cw, ch), focused);
    c.restore_clip(old);
}

fn render_window(w: &mut Window, focused: bool) {
    w.needs_render = false;
    w.render_area = None;
    if w.fullscreen.is_some() {
        let (cw, ch) = w.client_size();
        let mut c = w.surface.canvas();
        let old = c.push_clip(Rect::new(0, 0, cw, ch));
        w.app.render(&mut c, (cw, ch), focused);
        c.restore_clip(old);
        return;
    }
    let f = fonts();
    let title = w.app.title();
    let hover = w.hover_button;
    let (cw, ch) = w.client_size();
    let rw = w.rect.w;
    let mut c = w.surface.canvas();
    let tb = Rect::new(0, 0, rw, theme::TITLEBAR_H);
    c.fill_rect(tb, if focused { theme::titlebar() } else { theme::titlebar_inactive() });
    c.hline(0, theme::TITLEBAR_H - 1, rw, theme::separator());
    // App icon and title on the left (Linux style).
    let icon = w.app.icon();
    let mid = theme::TITLEBAR_H / 2;
    super::icons::draw(&mut c, icon, 12, mid - 9, 18);
    let tcol = if focused { theme::text() } else { theme::text_dim() };
    let base = (theme::TITLEBAR_H + f.bold.ascent - f.bold.descent) / 2;
    c.draw_text_clipped(&f.bold, 38, base, &title, rw - 38 - 120, tcol);
    // Minimise, maximise and close on the right: round buttons that light up.
    for b in 0..3u8 {
        let (cx, cy) = Window::button_center(b, rw);
        let hot = hover == Some(b);
        let (bg, fg) = match (b, hot) {
            (0, true) => (rgb(0xe8, 0x4a, 0x4f), rgb(255, 255, 255)),
            (_, true) => (rgba(0, 0, 0, 26), theme::text()),
            _ => (if focused { rgba(0, 0, 0, 12) } else { 0 }, if focused { theme::text() } else { theme::text_dim() }),
        };
        if bg != 0 {
            c.fill_circle(cx, cy, 11, bg);
        }
        match b {
            0 => {
                for d in -4..=4 {
                    c.blend_pixel(cx + d, cy + d, fg, 255);
                    c.blend_pixel(cx + d, cy - d, fg, 255);
                    c.blend_pixel(cx + d + 1, cy + d, fg, 110);
                    c.blend_pixel(cx + d + 1, cy - d, fg, 110);
                }
            }
            1 => c.fill_rect(Rect::new(cx - 5, cy + 1, 10, 2), fg),
            _ => c.stroke_rounded_rect(Rect::new(cx - 5, cy - 5, 10, 10), 2, 2, fg),
        }
    }

    c.translate(0, theme::TITLEBAR_H);
    let old = c.push_clip(Rect::new(0, 0, cw, ch));
    w.app.render(&mut c, (cw, ch), focused);
    c.restore_clip(old);
}
