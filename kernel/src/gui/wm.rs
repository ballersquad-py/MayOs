//! The window manager and compositor.
//!
//! Every window owns a surface holding its decorations and client area.
//! Compositing only touches "damaged" screen rectangles: the background,
//! shadows, rounded window surfaces, top bar and dock are painted in
//! z-order inside each damaged rectangle, then that rectangle is sent to
//! the display (for virtio-gpu: a transfer + flush of just that region).

use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use gfx::icons::{self, Icon};
use gfx::{rgb, rgba, with_alpha, Canvas, Rect, Surface};

use super::app::{App, AppEvent, AppKind, Command, Ctx, Msg, WindowId};
use super::display::{Display, CURSOR_DIM};
use super::theme::{self, fonts};
use crate::input::{InputEvent, Key, KeyEvent};

const RESIZE_BORDER: i32 = 7;
const DOUBLE_CLICK_MS: u64 = 450;

struct Window {
    id: WindowId,
    app: Box<dyn App>,
    rect: Rect,
    restore: Option<Rect>,
    minimized: bool,
    surface: Surface,
    needs_render: bool,
    hover_button: Option<u8>,
}

impl Window {
    fn maximized(&self) -> bool {
        self.restore.is_some()
    }

    fn radius(&self) -> i32 {
        if self.maximized() { 0 } else { theme::WINDOW_RADIUS }
    }

    /// Everything this window paints, including its shadow.
    fn paint_bounds(&self) -> Rect {
        if self.maximized() {
            return self.rect;
        }
        let b = theme::SHADOW_BLUR;
        Rect::new(self.rect.x - b, self.rect.y - b, self.rect.w + 2 * b, self.rect.h + 2 * b + theme::SHADOW_OFFSET)
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
    Explorer,
    Terminal,
    Editor,
    Restart,
    ShutDown,
}

const MENU: &[Option<(MenuItem, &str)>] = &[
    Some((MenuItem::About, "About MayOS")),
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
    menu_open: bool,
    menu_hover: Option<usize>,
    clock: String,
    cascade: i32,
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

fn render_background(w: i32, h: i32) -> Surface {
    let mut s = Surface::new(w, h, 0);
    {
        let mut c = s.canvas();
        c.fill_gradient_v(Rect::new(0, 0, w, h), rgb(0x1b, 0x2a, 0x5e), rgb(0x5a, 0x2d, 0x7a));
        // Soft glowing blobs for depth.
        let blobs = [
            (w * 3 / 4, h / 4, h / 2, rgba(0x6f, 0x8c, 0xff, 70)),
            (w / 5, h * 3 / 4, h * 2 / 5, rgba(0xff, 0x7e, 0xb6, 55)),
            (w / 2, h, h / 2, rgba(0x49, 0xc6, 0xe5, 45)),
        ];
        for (cx, cy, r, col) in blobs {
            let area = Rect::new(cx - r, cy - r, r * 2, r * 2);
            // Radial falloff: blend with strength decreasing with distance.
            for y in area.y.max(0)..area.bottom().min(h) {
                for x in area.x.max(0)..area.right().min(w) {
                    let dx = (x - cx) as i64;
                    let dy = (y - cy) as i64;
                    let d = gfx::isqrt((dx * dx + dy * dy) as u64) as i32;
                    if d >= r {
                        continue;
                    }
                    let t = (r - d) as u32 * 255 / r as u32;
                    let s = t * t / 255;
                    c.blend_pixel(x, y, col, s);
                }
            }
        }
    }
    s
}

impl Wm {
    pub fn new(mut display: Display, launcher: fn(AppKind) -> Option<Box<dyn App>>) -> Wm {
        let (width, height) = display.size();
        let (cursor, cursor_hot) = super::cursor::arrow();
        let pointer = (width / 2, height / 2);
        display.set_cursor(&cursor, cursor_hot, pointer);
        let mut wm = Wm {
            display,
            width,
            height,
            background: render_background(width, height),
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
            menu_open: false,
            menu_hover: None,
            clock: String::new(),
            cascade: 0,
            launcher,
        };
        wm.update_clock();
        wm.damage_all();
        wm
    }

    pub fn display_name(&self) -> &'static str {
        self.display.name()
    }

    pub fn size(&self) -> (i32, i32) {
        (self.width, self.height)
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

    // -----------------------------------------------------------------
    // Window lifecycle
    // -----------------------------------------------------------------

    pub fn open(&mut self, app: Box<dyn App>, over: Option<WindowId>) -> WindowId {
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
                let x = (self.width - w) / 2 - 120 + step * 32;
                let y = theme::TOPBAR_H + 40 + step * 28;
                (x, y)
            }
        };
        let x = x.clamp(8, (self.width - w - 8).max(8));
        let y = y.clamp(theme::TOPBAR_H + 8, (self.height - h_total - 8).max(theme::TOPBAR_H + 8));
        let id = self.next_id;
        self.next_id += 1;
        let rect = Rect::new(x, y, w, h_total);
        self.windows.push(Window {
            id,
            app,
            rect,
            restore: None,
            minimized: false,
            surface: Surface::new(w, h_total, theme::WINDOW_BG),
            needs_render: true,
            hover_button: None,
        });
        self.focus(Some(id));
        let b = self.windows.last().unwrap().paint_bounds();
        self.damage(b);
        id
    }

    pub fn open_kind(&mut self, kind: AppKind) {
        if let Some(app) = (self.launcher)(kind) {
            self.open(app, None);
        }
    }

    fn close(&mut self, id: WindowId) {
        let Some(i) = self.index_of(id) else { return };
        let w = self.windows.remove(i);
        self.damage(w.paint_bounds());
        if self.capture == Some(id) {
            self.capture = None;
        }
        if self.hovered == Some(id) {
            self.hovered = None;
        }
        if self.focused == Some(id) {
            self.focused = None;
            let next = self.windows.iter().rev().find(|w| !w.minimized).map(|w| w.id);
            self.focus(next);
        }
        self.damage(self.dock_rect());
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
            self.windows[i].minimized = false;
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

    fn minimize(&mut self, id: WindowId) {
        if let Some(i) = self.index_of(id) {
            self.windows[i].minimized = true;
            let b = self.windows[i].paint_bounds();
            self.damage(b);
            if self.focused == Some(id) {
                self.focused = None;
                let next = self.windows.iter().rev().find(|w| !w.minimized).map(|w| w.id);
                self.focus(next);
            }
        }
    }

    fn toggle_maximize(&mut self, id: WindowId) {
        let Some(i) = self.index_of(id) else { return };
        if !self.windows[i].app.resizable() {
            return;
        }
        let old = self.windows[i].paint_bounds();
        let new = match self.windows[i].restore.take() {
            Some(r) => r,
            None => {
                self.windows[i].restore = Some(self.windows[i].rect);
                Rect::new(0, theme::TOPBAR_H, self.width, self.height - theme::TOPBAR_H)
            }
        };
        self.set_rect(i, new);
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

    // -----------------------------------------------------------------
    // Geometry of shell elements
    // -----------------------------------------------------------------

    fn dock_rect(&self) -> Rect {
        let n = DOCK.len() as i32;
        let w = n * theme::DOCK_ICON + (n + 1) * theme::DOCK_PAD + 8;
        let h = theme::DOCK_ICON + theme::DOCK_PAD * 2;
        // Extra room above for the tooltip.
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
        Rect::new(d.x - 60, d.y - 40, d.w + 120, d.h + 50)
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
        let t = crate::arch::rtc::now();
        let s = format!("{} {} {}   {:02}:{:02}", weekday(t.year, t.month, t.day), month_name(t.month), t.day, t.hour, t.minute);
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
                // Simple acceleration for relative mice.
                let accel = |d: i32| if d.abs() > 6 { d * 2 } else { d };
                let (x, y) = (self.pointer.0 + accel(dx), self.pointer.1 + accel(dy));
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
            InputEvent::Wheel(d) => self.wheel(d),
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
        // Hover tracking for windows.
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
            .filter(|w| !w.minimized)
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
                        // Dragging a maximized window restores it under the pointer.
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

        let Some(id) = self.window_at(x, y) else {
            return;
        };
        let i = self.index_of(id).unwrap();
        self.focus(Some(id));
        let i = self.index_of(id).unwrap_or(i);
        let r = self.windows[i].rect;
        let (lx, ly) = (x - r.x, y - r.y);

        // Resize edges.
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
            let now = crate::time::uptime_ms();
            let double = now - self.last_click.0 < DOUBLE_CLICK_MS
                && (x - self.last_click.1).abs() < 5
                && (y - self.last_click.2).abs() < 5;
            self.last_click = (now, x, y, 1);
            if double {
                self.toggle_maximize(id);
                self.last_click.0 = 0;
                return;
            }
            self.drag = Some(Drag::Move { id, dx: lx, dy: ly });
            return;
        }

        let now = crate::time::uptime_ms();
        let clicks = if now - self.last_click.0 < DOUBLE_CLICK_MS
            && (x - self.last_click.1).abs() < 5
            && (y - self.last_click.2).abs() < 5
        {
            self.last_click.3.saturating_add(1)
        } else {
            1
        };
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
            if k.alt && k.key == Key::F(4) {
                if let Some(id) = self.focused {
                    self.request_close(id);
                }
                return;
            }
            if k.alt && k.key == Key::Tab {
                // Cycle focus through visible windows.
                if let Some(w) = self.windows.iter().find(|w| !w.minimized).map(|w| w.id) {
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
        self.damage(self.menu_rect());
    }

    fn menu_action(&mut self, item: MenuItem) {
        match item {
            MenuItem::About => self.dock_click(AppKind::About, false),
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
                self.windows.iter().filter(|w| w.app.kind() == kind).map(|w| w.id).collect();
            if !existing.is_empty() {
                // If the top window of this kind is already focused, cycle.
                let target = if existing.len() > 1 && self.focused == existing.last().copied() {
                    existing[0]
                } else {
                    *existing.last().unwrap()
                };
                if let Some(i) = self.index_of(target) {
                    self.windows[i].minimized = false;
                    let b = self.windows[i].paint_bounds();
                    self.damage(b);
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

    /// Tick apps, re-render dirty windows and composite damaged regions.
    pub fn frame(&mut self) {
        for i in 0..self.windows.len() {
            if i >= self.windows.len() {
                break;
            }
            let id = self.windows[i].id;
            let mut ctx = Ctx::new(id);
            self.windows[i].app.tick(&mut ctx);
            self.apply(ctx);
        }
        if self.update_clock() {
            self.damage(Rect::new(self.width - 260, 0, 260, theme::TOPBAR_H));
        }

        let focused = self.focused;
        for i in 0..self.windows.len() {
            if !self.windows[i].needs_render {
                continue;
            }
            let is_focused = focused == Some(self.windows[i].id);
            render_window(&mut self.windows[i], is_focused);
            if !self.windows[i].minimized {
                let b = self.windows[i].paint_bounds();
                self.damage(b);
            }
        }

        if self.cursor_dirty {
            self.cursor_dirty = false;
            self.display.move_cursor(self.pointer.0, self.pointer.1);
        }
        self.composite();
    }

    fn merged_damage(&mut self) -> Vec<Rect> {
        let mut rects: Vec<Rect> = core::mem::take(&mut self.damage);
        // Merge overlapping rectangles until stable.
        let mut changed = true;
        while changed {
            changed = false;
            let mut i = 0;
            while i < rects.len() {
                let mut j = i + 1;
                while j < rects.len() {
                    let a = rects[i];
                    let b = rects[j];
                    let grown = a.inset(-16);
                    if grown.intersects(&b) {
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
                for win in windows.iter().filter(|w| !w.minimized) {
                    if !win.paint_bounds().intersects(&r) {
                        continue;
                    }
                    let radius = win.radius();
                    if !win.maximized() {
                        let shadow = if focused_is(*focused, win.id) { theme::SHADOW } else { theme::SHADOW_INACTIVE };
                        c.draw_shadow(win.rect.offset(0, theme::SHADOW_OFFSET), radius, theme::SHADOW_BLUR, shadow);
                    }
                    c.blit_rounded(&win.surface, win.rect.x, win.rect.y, radius);
                    if !win.maximized() {
                        c.stroke_rounded_rect(win.rect, radius, 1, theme::BORDER);
                    }
                }
            }
            self.paint_shell(r);
            self.display.present(r);
        }
    }

    /// Top bar, dock, menu and (if needed) the software cursor.
    fn paint_shell(&mut self, r: Rect) {
        let (w, h) = (self.width, self.height);
        let focused_title = self
            .focused
            .and_then(|id| self.windows.iter().find(|x| x.id == id))
            .map(|x| x.app.title())
            .unwrap_or_default();
        let running: Vec<AppKind> = self.windows.iter().map(|w| w.app.kind()).collect();
        let dock = self.dock_rect();
        let icon_rects: Vec<Rect> = (0..DOCK.len()).map(|i| self.dock_icon_rect(i)).collect();
        let menu_rect = self.menu_rect();
        let logo = self.logo_rect();
        let f = fonts();
        let Wm { display, clock, dock_hover, menu_open, menu_hover, pointer, cursor, cursor_hot, .. } = self;
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
            c.fill_circle(logo.x + 14, logo.y + logo.h / 2, 7, theme::ACCENT);
            c.fill_circle(logo.x + 14, logo.y + logo.h / 2, 3, rgb(255, 255, 255));
            let base = (theme::TOPBAR_H + f.bold.ascent - f.bold.descent) / 2;
            c.draw_text(&f.bold, logo.x + 28, base, "MayOS", theme::TEXT);
            if !focused_title.is_empty() {
                c.draw_text_clipped(&f.ui, logo.right() + 16, base, &focused_title, w / 2 - logo.right(), theme::TEXT);
            }
            let cw = f.ui.measure(clock);
            c.draw_text(&f.ui, w - cw - 16, base, clock, theme::TEXT);
        }

        // Dock.
        let dock_area = Rect::new(dock.x - 60, dock.y - 40, dock.w + 120, dock.h + 50);
        if dock_area.intersects(&r) {
            c.draw_shadow(dock, 20, 18, rgba(0, 0, 0, 60));
            c.fill_rounded_rect(dock, 20, rgba(255, 255, 255, 165));
            c.stroke_rounded_rect(dock, 20, 1, rgba(255, 255, 255, 200));
            for (i, (kind, icon, name)) in DOCK.iter().enumerate() {
                let ir = icon_rects[i];
                let lift = if *dock_hover == Some(i) { 4 } else { 0 };
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

        // Menu.
        if *menu_open && menu_rect.intersects(&r) {
            c.draw_shadow(menu_rect, 10, 16, rgba(0, 0, 0, 80));
            c.fill_rounded_rect(menu_rect, 10, rgba(250, 251, 253, 245));
            c.stroke_rounded_rect(menu_rect, 10, 1, rgba(0, 0, 0, 40));
            let mut y = menu_rect.y + 6;
            for (i, m) in MENU.iter().enumerate() {
                match m {
                    Some((_, label)) => {
                        let row = Rect::new(menu_rect.x + 6, y, menu_rect.w - 12, 28);
                        let hovered = *menu_hover == Some(i);
                        if hovered {
                            c.fill_rounded_rect(row, 6, theme::ACCENT);
                        }
                        let col = if hovered { rgb(255, 255, 255) } else { theme::TEXT };
                        let base = row.y + (row.h + f.ui.ascent - f.ui.descent) / 2;
                        c.draw_text(&f.ui, row.x + 12, base, label, col);
                        y += 28;
                    }
                    None => {
                        c.hline(menu_rect.x + 12, y + 4, menu_rect.w - 24, theme::SEPARATOR);
                        y += 9;
                    }
                }
            }
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

    pub fn window_count(&self) -> usize {
        self.windows.len()
    }
}

fn focused_is(f: Option<WindowId>, id: WindowId) -> bool {
    f == Some(id)
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
