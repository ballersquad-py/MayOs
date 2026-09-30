//! The Settings app: display, personalisation, sound, network, mouse &
//! keyboard, date & time, storage and about pages.
//!
//! Layout is immediate-mode: `render` draws the current page and records
//! a hit rectangle for every interactive control, and events are matched
//! against the rectangles from the last frame.

use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;

use gfx::icons::{self, Icon};
use gfx::{mix, rgb, with_alpha, Canvas, Color, Rect, Surface};

use super::app::{App, AppEvent, AppKind, Command, Ctx, Msg};
use super::theme::{self, fonts};
use super::wallpaper::{self, WALLPAPERS};
use super::widgets::{button, ButtonStyle, TextInput};
use crate::audio::{self, SystemSound};
use crate::fs;
use crate::input::Key;
use crate::settings::{self, Settings};
use crate::sync::Spin;
use crate::time::uptime_ms;

const SIDEBAR_W: i32 = 214;
const ROW_H: i32 = 58;
const TOGGLE_MS: u64 = 160;
const TAG_SETUP_DISK: u32 = 40;
const PAGE_MS: u64 = 200;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Page {
    Display,
    Personalization,
    Sound,
    Network,
    Input,
    DateTime,
    Storage,
    About,
}

const PAGES: &[(Page, &str, Color)] = &[
    (Page::Display, "Display", rgb(0x2f, 0x7c, 0xf6)),
    (Page::Personalization, "Personalization", rgb(0xe0, 0x4f, 0x92)),
    (Page::Sound, "Sound", rgb(0xf0, 0x8a, 0x24)),
    (Page::Network, "Network & Internet", rgb(0x14, 0x9e, 0xa8)),
    (Page::Input, "Mouse & Keyboard", rgb(0x6e, 0x74, 0x80)),
    (Page::DateTime, "Date & Time", rgb(0x8e, 0x5c, 0xe6)),
    (Page::Storage, "Storage", rgb(0x2f, 0xa8, 0x5a)),
    (Page::About, "About", rgb(0x4a, 0x55, 0x68)),
];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Toggle {
    Animations,
    DarkMode,
    Muted,
    SystemSounds,
    NaturalScroll,
    Clock24h,
    ShowSeconds,
    Dhcp,
    DockAutohide,
    FileSharing,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Slider {
    Volume,
    PointerSpeed,
    DoubleClick,
    KeyRepeat,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Action {
    Page(Page),
    Toggle(Toggle),
    Slider(Slider),
    Resolution(u32, u32),
    OpenResolutions,
    Theme(bool),
    Wallpaper(usize),
    Picture(usize),
    DockPosition(u8),
    DockSize(u8),
    Accent(usize),
    TzMinus,
    TzPlus,
    TestSound,
    RenewDhcp,
    ApplyStatic,
    PingGateway,
    PingInternet,
    LookupName,
    Input(usize),
    ResetSettings,
    SetupDisk(usize),
}

fn aspect(w: u32, h: u32) -> &'static str {
    match (w, h) {
        (1920, 1080) | (1280, 720) | (1366, 768) | (1600, 900) | (2560, 1440) | (3840, 2160) => "16:9",
        (1280, 800) | (1440, 900) | (1680, 1050) | (1920, 1200) | (2560, 1600) => "16:10",
        (1024, 768) | (800, 600) | (1152, 864) | (1600, 1200) => "4:3",
        (1280, 1024) => "5:4",
        (2560, 1080) | (3440, 1440) => "21:9",
        _ => "",
    }
}

fn toggle_value(s: &Settings, t: Toggle) -> bool {
    match t {
        Toggle::Animations => s.animations,
        Toggle::DarkMode => s.dark,
        Toggle::Muted => s.muted,
        Toggle::SystemSounds => s.system_sounds,
        Toggle::NaturalScroll => s.natural_scroll,
        Toggle::Clock24h => s.clock_24h,
        Toggle::ShowSeconds => s.show_seconds,
        Toggle::Dhcp => s.dhcp,
        Toggle::DockAutohide => s.dock_autohide,
        Toggle::FileSharing => s.file_sharing,
    }
}

fn set_toggle(s: &mut Settings, t: Toggle, v: bool) {
    match t {
        Toggle::Animations => s.animations = v,
        Toggle::DarkMode => s.dark = v,
        Toggle::Muted => s.muted = v,
        Toggle::SystemSounds => s.system_sounds = v,
        Toggle::NaturalScroll => s.natural_scroll = v,
        Toggle::Clock24h => s.clock_24h = v,
        Toggle::ShowSeconds => s.show_seconds = v,
        Toggle::Dhcp => s.dhcp = v,
        Toggle::DockAutohide => s.dock_autohide = v,
        Toggle::FileSharing => s.file_sharing = v,
    }
}

/// (min, max) of each slider.
fn slider_range(sl: Slider) -> (i32, i32) {
    match sl {
        Slider::Volume => (0, 100),
        Slider::PointerSpeed => (1, 10),
        Slider::DoubleClick => (200, 900),
        Slider::KeyRepeat => (1, 5),
    }
}

fn slider_value(s: &Settings, sl: Slider) -> i32 {
    match sl {
        Slider::Volume => s.volume as i32,
        Slider::PointerSpeed => s.pointer_speed as i32,
        Slider::DoubleClick => s.double_click_ms as i32,
        Slider::KeyRepeat => s.key_repeat as i32,
    }
}

fn set_slider(s: &mut Settings, sl: Slider, v: i32) {
    match sl {
        Slider::Volume => s.volume = v as u8,
        Slider::PointerSpeed => s.pointer_speed = v as u8,
        Slider::DoubleClick => s.double_click_ms = v as u32,
        Slider::KeyRepeat => s.key_repeat = v as u8,
    }
}

/// Result of the background network test, shown on the Network page.
static NET_TEST: Spin<Option<(String, bool)>> = Spin::new(None);

extern "C" fn net_test_thread(kind: usize) {
    let result = match kind {
        0 | 1 => {
            let target = if kind == 0 {
                crate::network::status().map(|s| s.gateway).unwrap_or_default()
            } else {
                net::Ipv4([1, 1, 1, 1])
            };
            let mut replies = Vec::new();
            let mut err = None;
            for seq in 0..3 {
                match crate::network::ping(target, seq, 2000) {
                    Ok(us) => replies.push(us),
                    Err(e) => err = Some(e),
                }
            }
            if replies.is_empty() {
                (format!("Ping {}: {}", target, err.map(|e| e.to_string()).unwrap_or_default()), false)
            } else {
                let avg = replies.iter().sum::<u64>() / replies.len() as u64;
                (format!("Ping {}: {}/3 replies, average {}.{} ms", target, replies.len(), avg / 1000, (avg % 1000) / 100), true)
            }
        }
        _ => match crate::network::resolve("example.com") {
            Ok(ip) => (format!("example.com resolves to {}", ip), true),
            Err(e) => (format!("Looking up example.com failed: {}", e), false),
        },
    };
    *NET_TEST.lock() = Some(result);
}

pub struct SettingsApp {
    page: Page,
    page_since: u64,
    size: (i32, i32),
    hits: Vec<(Rect, Action)>,
    hover: Option<Action>,
    cfg: Settings,
    cfg_gen: u64,
    /// Toggle knob animation: (toggle, from 0..1024, start time).
    knobs: Vec<(Toggle, i64, u64)>,
    dragging: Option<(Slider, Rect)>,
    inputs: [TextInput; 4],
    focused_input: Option<usize>,
    message: Option<(String, bool, u64)>,
    net_test_running: bool,
    thumbs: Vec<Surface>,
    pictures: Option<Arc<Spin<PictureScan>>>,
    picture_thumbs: Vec<(String, Option<Surface>)>,
    last_refresh: u64,
    scroll: i32,
    bottom: core::cell::Cell<i32>,
    /// Disks offered on the Storage page: (target, description, erases data).
    disk_targets: Vec<(crate::storage::Target, String, bool)>,
    pending_setup: Option<crate::storage::Target>,
    /// The open dropdown's button (its list is drawn over the page).
    dropdown: Option<Rect>,
}

impl SettingsApp {
    pub fn new() -> SettingsApp {
        let cfg = settings::get();
        let inputs = [
            TextInput::new(&cfg.static_ip),
            TextInput::new(&cfg.static_mask),
            TextInput::new(&cfg.static_gateway),
            TextInput::new(&cfg.static_dns),
        ];
        SettingsApp {
            page: Page::Display,
            page_since: 0,
            size: (860, 560),
            hits: Vec::new(),
            hover: None,
            cfg_gen: settings::generation(),
            cfg,
            knobs: Vec::new(),
            dragging: None,
            inputs,
            focused_input: None,
            message: None,
            net_test_running: false,
            thumbs: Vec::new(),
            pictures: None,
            picture_thumbs: Vec::new(),
            last_refresh: 0,
            scroll: 0,
            bottom: core::cell::Cell::new(0),
            disk_targets: Vec::new(),
            pending_setup: None,
            dropdown: None,
        }
    }

    fn flash(&mut self, m: String, ok: bool) {
        self.message = Some((m, ok, uptime_ms() + 5000));
    }

    fn set_page(&mut self, p: Page) {
        if p != self.page {
            self.page = p;
            self.page_since = if self.cfg.animations { uptime_ms() } else { 0 };
            self.focused_input = None;
            self.scroll = 0;
            self.dropdown = None;
        }
    }

    fn knob_pos(&self, t: Toggle) -> i64 {
        let target = if toggle_value(&self.cfg, t) { 1024 } else { 0 };
        if let Some(&(_, from, start)) = self.knobs.iter().find(|k| k.0 == t) {
            let p = ((uptime_ms() - start) as i64 * 1024 / TOGGLE_MS as i64).min(1024);
            let e = 1024 - (1024 - p) * (1024 - p) / 1024;
            return from + (target - from) * e / 1024;
        }
        target
    }

    fn animating(&self) -> bool {
        let now = uptime_ms();
        self.knobs.iter().any(|k| now - k.2 < TOGGLE_MS + 20) || (self.page_since != 0 && now - self.page_since < PAGE_MS + 20)
    }

    // ---------------------------------------------------------------
    // Drawing helpers
    // ---------------------------------------------------------------

    fn hovered(&self, a: Action) -> bool {
        self.hover == Some(a)
    }

    /// Remember how far down the page content reaches (for scrolling).
    fn extend(&self, bottom: i32) {
        if bottom > self.bottom.get() {
            self.bottom.set(bottom);
        }
    }

    fn card(&self, c: &mut Canvas, r: Rect) {
        self.extend(r.bottom());
        c.fill_rounded_rect(r, 12, theme::card_bg());
        c.stroke_rounded_rect(r, 12, 1, theme::border());
    }

    /// Draw a labelled row inside a card; returns the row rect.
    fn row(&self, c: &mut Canvas, card: Rect, i: i32, label: &str, detail: Option<&str>, last: bool) -> Rect {
        let f = fonts();
        let r = Rect::new(card.x, card.y + i * ROW_H, card.w, ROW_H);
        match detail {
            Some(d) => {
                c.draw_text(&f.ui, r.x + 18, r.y + 22, label, theme::text());
                c.draw_text_clipped(&f.ui, r.x + 18, r.y + 40, d, r.w / 2, theme::text_dim());
            }
            None => {
                let base = r.y + (ROW_H + f.ui.ascent - f.ui.descent) / 2;
                c.draw_text(&f.ui, r.x + 18, base, label, theme::text());
            }
        }
        if !last {
            c.hline(r.x + 18, r.bottom() - 1, r.w - 36, theme::separator());
        }
        r
    }

    /// Right-aligned value text in a row.
    fn value(&self, c: &mut Canvas, row: Rect, text: &str) {
        let f = &fonts().ui;
        let w = f.measure(text).min(row.w / 2);
        let base = row.y + (ROW_H + f.ascent - f.descent) / 2;
        c.draw_text_clipped(f, row.right() - 18 - w, base, text, row.w / 2, theme::text_dim());
    }

    fn toggle(&mut self, c: &mut Canvas, row: Rect, t: Toggle) {
        let r = Rect::new(row.right() - 18 - 44, row.y + (ROW_H - 26) / 2, 44, 26);
        let pos = self.knob_pos(t);
        let off = theme::control_off();
        let track = mix(off, theme::accent(), (pos * 255 / 1024) as u32);
        c.fill_rounded_rect(r, 13, track);
        let kx = r.x + 3 + ((r.w - 26) as i64 * pos / 1024) as i32;
        c.fill_circle(kx + 10, r.y + 13, 11, theme::shade(35));
        c.fill_circle(kx + 10, r.y + 13, 10, rgb(255, 255, 255));
        self.hits.push((row, Action::Toggle(t)));
    }

    fn slider(&mut self, c: &mut Canvas, row: Rect, sl: Slider, label: &str) {
        let f = &fonts().ui;
        let (lo, hi) = slider_range(sl);
        let v = slider_value(&self.cfg, sl);
        let track = Rect::new(row.right() - 18 - 240, row.y + ROW_H / 2 - 3, 200, 6);
        let t = ((v - lo) * track.w / (hi - lo).max(1)).clamp(0, track.w);
        c.fill_rounded_rect(track, 3, theme::control_off());
        c.fill_rounded_rect(Rect::new(track.x, track.y, t.max(6), track.h), 3, theme::accent());
        let active = self.dragging.map(|d| d.0) == Some(sl) || self.hovered(Action::Slider(sl));
        c.fill_circle(track.x + t, track.y + 3, if active { 10 } else { 9 }, theme::shade(40));
        c.fill_circle(track.x + t, track.y + 3, if active { 9 } else { 8 }, rgb(255, 255, 255));
        let base = row.y + (ROW_H + f.ascent - f.descent) / 2;
        c.draw_text(f, track.right() + 14, base, label, theme::text_dim());
        self.hits.push((Rect::new(track.x - 12, row.y, track.w + 24, ROW_H), Action::Slider(sl)));
    }

    fn action_button(&mut self, c: &mut Canvas, r: Rect, label: &str, style: ButtonStyle, a: Action) {
        self.extend(r.bottom());
        button(c, r, label, style, self.hovered(a), true);
        self.hits.push((r, a));
    }

    fn note(&self, c: &mut Canvas, x: i32, y: i32, w: i32, text: &str) -> i32 {
        // Simple word wrap.
        let f = &fonts().ui;
        let mut line = String::new();
        let mut yy = y;
        for word in text.split(' ') {
            let candidate = if line.is_empty() { word.to_string() } else { format!("{} {}", line, word) };
            if f.measure(&candidate) > w && !line.is_empty() {
                c.draw_text(f, x, yy, &line, theme::text_dim());
                yy += 18;
                line = word.to_string();
            } else {
                line = candidate;
            }
        }
        if !line.is_empty() {
            c.draw_text(f, x, yy, &line, theme::text_dim());
            yy += 18;
        }
        self.extend(yy);
        yy
    }

    fn sidebar_icon(c: &mut Canvas, page: Page, _color: Color, x: i32, y: i32) {
        let icon = match page {
            Page::Display => Icon::SetDisplay,
            Page::Personalization => Icon::SetTheme,
            Page::Sound => Icon::SetSound,
            Page::Network => Icon::SetNetwork,
            Page::Input => Icon::SetMouse,
            Page::DateTime => Icon::SetTime,
            Page::Storage => Icon::Drive,
            Page::About => Icon::MayOS,
        };
        super::icons::draw(c, icon, x - 1, y - 1, 28);
    }

    // ---------------------------------------------------------------
    // Pages
    // ---------------------------------------------------------------

    fn page_title(&self, c: &mut Canvas, x: i32, y: i32, title: &str) -> i32 {
        c.draw_text(&fonts().large, x, y + 28, title, theme::text());
        y + 62
    }

    /// A dropdown button at the right end of a row.
    fn dropdown_button(&mut self, c: &mut Canvas, row: Rect, text: &str, a: Action) -> Rect {
        let f = fonts();
        let bw = (f.ui.measure(text) + 52).max(180);
        let r = Rect::new(row.right() - 18 - bw, row.y + (ROW_H - 34) / 2, bw, 34);
        let open = self.dropdown == Some(r);
        c.fill_rounded_rect(r, 8, if self.hovered(a) || open { theme::hover() } else { theme::panel_bg() });
        c.stroke_rounded_rect(r, 8, 1, if open { theme::accent() } else { theme::border() });
        let base = r.y + (r.h + f.ui.ascent - f.ui.descent) / 2;
        c.draw_text(&f.ui, r.x + 14, base, text, theme::text());
        // Chevron.
        let (cx, cy) = (r.right() - 20, r.y + r.h / 2 - 2);
        for k in 0..5 {
            c.fill_rect(Rect::new(cx - 5 + k, cy + k, 11 - 2 * k, 1), theme::text_dim());
        }
        self.hits.push((r, a));
        r
    }

    /// The open list of resolutions, over everything else.
    fn resolution_list(&mut self, c: &mut Canvas) {
        let Some(b) = self.dropdown else { return };
        let f = fonts();
        let modes = super::display_modes();
        let current = super::display_mode();
        let item_h = 34;
        let h = (modes.len() as i32 * item_h + 8).min(self.size.1 - 40);
        let below = b.bottom() + 6 + h <= self.size.1 - 8;
        let r = Rect::new(b.x, if below { b.bottom() + 6 } else { (b.y - 6 - h).max(8) }, b.w, h);
        // Clicking anywhere else closes it.
        self.hits.push((Rect::new(0, 0, self.size.0, self.size.1), Action::OpenResolutions));
        c.draw_shadow(r, 10, 16, with_alpha(0x000000, 70));
        c.fill_rounded_rect(r, 10, theme::card_bg());
        c.stroke_rounded_rect(r, 10, 1, theme::border());
        let old = c.push_clip(r.inset(4));
        for (i, &(mw, mh)) in modes.iter().enumerate() {
            let ir = Rect::new(r.x + 4, r.y + 4 + i as i32 * item_h, r.w - 8, item_h);
            let a = Action::Resolution(mw, mh);
            let sel = (mw, mh) == current;
            if sel {
                c.fill_rounded_rect(ir, 7, with_alpha(theme::accent(), 40));
            } else if self.hovered(a) {
                c.fill_rounded_rect(ir, 7, theme::hover());
            }
            let base = ir.y + (ir.h + f.ui.ascent - f.ui.descent) / 2;
            c.draw_text(if sel { &f.bold } else { &f.ui }, ir.x + 12, base, &format!("{} \u{00d7} {}", mw, mh), theme::text());
            let ratio = aspect(mw, mh);
            let rw = f.ui.measure(ratio);
            c.draw_text(&f.ui, ir.right() - 12 - rw, base, ratio, theme::text_dim());
            self.hits.push((ir, a));
        }
        c.restore_clip(old);
    }

    fn section(&self, c: &mut Canvas, x: i32, y: i32, title: &str) -> i32 {
        c.draw_text(&fonts().small_bold, x + 4, y, &title.to_ascii_uppercase(), theme::text_dim());
        y + 12
    }

    fn page_display(&mut self, c: &mut Canvas, x: i32, mut y: i32, w: i32) {
        y = self.page_title(c, x, y, "Display");
        let modes = super::display_modes();
        let (mw, mh) = super::display_mode();
        y = self.section(c, x, y, "Screen");
        let card = Rect::new(x, y, w, ROW_H * 4);
        self.card(c, card);
        let r = self.row(c, card, 0, "Resolution", Some(aspect(mw, mh)), false);
        if modes.len() > 1 {
            self.dropdown_button(c, r, &format!("{} \u{00d7} {}", mw, mh), Action::OpenResolutions);
        } else {
            self.value(c, r, &format!("{} \u{00d7} {} (fixed)", mw, mh));
        }
        let r = self.row(c, card, 1, "Refresh rate", Some("Chosen by the screen's firmware"), false);
        self.value(c, r, "Monitor default");
        let r = self.row(c, card, 2, "Graphics", None, false);
        self.value(c, r, &super::display_description());
        let r = self.row(c, card, 3, "Animations", Some("Window, panel and menu motion"), true);
        self.toggle(c, r, Toggle::Animations);
        y = card.bottom() + 14;
        let note = if modes.len() <= 1 {
            "This screen uses the firmware's boot picture, which keeps one resolution and refresh rate. Choosing \
             them needs a driver for the graphics card (AMD, NVIDIA and Intel GPUs are not supported yet); in \
             VirtualBox, QEMU and VMware MayOS can change resolution."
        } else {
            "Refresh rate follows the screen; changing it needs a native driver for the graphics card \
             (not available yet for AMD, NVIDIA and Intel GPUs)."
        };
        self.note(c, x + 4, y + 4, w - 8, note);
    }

    fn page_personalization(&mut self, c: &mut Canvas, x: i32, mut y: i32, w: i32) {
        let f = fonts();
        y = self.page_title(c, x, y, "Personalization");
        y = self.section(c, x, y, "Appearance");
        // Light / dark previews.
        let card = Rect::new(x, y, w, 176);
        self.card(c, card);
        let pw = ((w - 36 - 16) / 2).min(220);
        for (k, dark) in [false, true].into_iter().enumerate() {
            let r = Rect::new(x + 18 + k as i32 * (pw + 16), y + 16, pw, 116);
            let a = Action::Theme(dark);
            let sel = self.cfg.dark == dark;
            if sel {
                c.fill_rounded_rect(r.inset(-3), 12, theme::accent());
            } else if self.hovered(a) {
                c.fill_rounded_rect(r.inset(-3), 12, theme::shade(30));
            }
            let (bg, win, bar, txt) = if dark {
                (rgb(0x16, 0x18, 0x1d), rgb(0x1f, 0x21, 0x26), rgb(0x2a, 0x2d, 0x33), rgb(0x55, 0x5b, 0x66))
            } else {
                (rgb(0xdd, 0xe3, 0xec), rgb(0xff, 0xff, 0xff), rgb(0xf0, 0xf1, 0xf4), rgb(0xc8, 0xcd, 0xd5))
            };
            c.fill_rounded_rect(r, 10, bg);
            let wr = Rect::new(r.x + 16, r.y + 14, r.w - 32, r.h - 28);
            c.fill_rounded_rect(wr, 7, win);
            c.fill_rounded_rect(Rect::new(wr.x, wr.y, wr.w, 16), 7, bar);
            c.fill_rect(Rect::new(wr.x, wr.y + 10, wr.w, 6), bar);
            c.fill_rounded_rect(Rect::new(wr.x + 10, wr.y + 26, wr.w / 2, 7), 3, txt);
            c.fill_rounded_rect(Rect::new(wr.x + 10, wr.y + 40, wr.w / 3, 7), 3, txt);
            c.fill_rounded_rect(Rect::new(wr.x + 10, wr.y + 56, 40, 12), 6, theme::accent());
            let label = if dark { "Dark" } else { "Light" };
            c.draw_text_centered(if sel { &f.bold } else { &f.ui }, Rect::new(r.x, r.bottom() + 8, r.w, 20), label, theme::text());
            self.hits.push((r, a));
        }
        y = card.bottom() + 12;
        let card = Rect::new(x, y, w, ROW_H * 3);
        self.card(c, card);
        let r = self.row(c, card, 0, "Accent colour", Some(theme::ACCENTS[self.cfg.accent.min(theme::ACCENTS.len() - 1)].0), false);
        let n = theme::ACCENTS.len() as i32;
        for (i, (_, col)) in theme::ACCENTS.iter().enumerate() {
            let cx = r.right() - 30 - (n - 1 - i as i32) * 34;
            let cy = r.y + ROW_H / 2;
            let hr = Rect::new(cx - 15, cy - 15, 30, 30);
            if self.cfg.accent == i {
                c.fill_circle(cx, cy, 14, *col);
                c.fill_circle(cx, cy, 11, theme::card_bg());
            } else if self.hovered(Action::Accent(i)) {
                c.fill_circle(cx, cy, 14, with_alpha(*col, 90));
            }
            c.fill_circle(cx, cy, 9, *col);
            self.hits.push((hr, Action::Accent(i)));
        }
        let r = self.row(c, card, 1, "Animations", Some("Window, panel and menu motion"), false);
        self.toggle(c, r, Toggle::Animations);
        let r = self.row(c, card, 2, "Hide the panel automatically", Some("Point at the bottom edge to show it"), true);
        self.toggle(c, r, Toggle::DockAutohide);
        y = card.bottom() + 26;
        y = self.section(c, x, y, "Wallpaper");
        if self.thumbs.len() != WALLPAPERS.len() {
            self.thumbs = (0..WALLPAPERS.len()).map(|i| wallpaper::render(i, 160, 100)).collect();
        }
        let cols = ((w + 14) / 140).max(2);
        let tw = (w - (cols - 1) * 16) / cols;
        let th = tw * 10 / 16;
        for (i, wp) in WALLPAPERS.iter().enumerate() {
            let (col, rowi) = (i as i32 % cols, i as i32 / cols);
            let r = Rect::new(x + col * (tw + 16), y + rowi * (th + 34), tw, th);
            let selected = self.cfg.wallpaper == i && self.cfg.wallpaper_image.is_empty();
            if selected {
                c.fill_rounded_rect(r.inset(-4), 14, theme::accent());
            } else if self.hovered(Action::Wallpaper(i)) {
                c.fill_rounded_rect(r.inset(-4), 14, theme::shade(30));
            }
            c.blit_scaled(&self.thumbs[i], r, 255, 10);
            c.draw_text_centered(&f.ui, Rect::new(r.x, r.bottom() + 4, r.w, 20), wp.name, if selected { theme::text() } else { theme::text_dim() });
            self.hits.push((r, Action::Wallpaper(i)));
            self.extend(r.bottom() + 24);
        }
        let rows = (WALLPAPERS.len() as i32 + cols - 1) / cols;
        y += rows * (th + 34) + 18;
        y = self.pictures_section(c, x, y, w, cols, tw, th);
        let _ = y;
    }

    /// A row of mutually exclusive buttons at the right end of a row.
    fn segmented(&mut self, c: &mut Canvas, row: Rect, labels: &[&str], selected: u8, action: fn(u8) -> Action) {
        let f = fonts();
        let seg_w = labels.iter().map(|l| f.ui.measure(l)).max().unwrap_or(40) + 24;
        let total = seg_w * labels.len() as i32 + 4;
        let outer = Rect::new(row.right() - 18 - total, row.y + (ROW_H - 32) / 2, total, 32);
        c.fill_rounded_rect(outer, 9, theme::separator());
        for (i, label) in labels.iter().enumerate() {
            let r = Rect::new(outer.x + 2 + i as i32 * seg_w, outer.y + 2, seg_w, 28);
            let a = action(i as u8);
            if selected == i as u8 {
                c.draw_shadow(r, 7, 4, with_alpha(0x000000, 40));
                c.fill_rounded_rect(r, 7, theme::card_bg());
            } else if self.hovered(a) {
                c.fill_rounded_rect(r, 7, theme::shade(12));
            }
            let col = if selected == i as u8 { theme::text() } else { theme::text_dim() };
            c.draw_text_centered(&f.ui, r, label, col);
            self.hits.push((r, a));
        }
    }

    /// "Your pictures": every PNG/JPEG/BMP in /pictures, /home and the top
    /// level (and /pictures folder) of other disks, as wallpaper choices.
    fn pictures_section(&mut self, c: &mut Canvas, x: i32, mut y: i32, w: i32, cols: i32, tw: i32, th: i32) -> i32 {
        let f = fonts();
        y = self.section(c, x, y, "Your pictures");
        if self.pictures.is_none() {
            self.pictures = Some(start_picture_scan());
            self.picture_thumbs.clear();
        }
        let scanning = self.pictures.as_ref().map(|p| !p.lock().done).unwrap_or(false);
        for (i, (path, thumb)) in self.picture_thumbs.iter().enumerate() {
            let (col, rowi) = (i as i32 % cols, i as i32 / cols);
            let r = Rect::new(x + col * (tw + 16), y + rowi * (th + 34), tw, th);
            let selected = self.cfg.wallpaper_image == *path;
            if selected {
                c.fill_rounded_rect(r.inset(-4), 14, theme::accent());
            } else if self.hovered(Action::Picture(i)) {
                c.fill_rounded_rect(r.inset(-4), 14, theme::shade(30));
            }
            match thumb {
                Some(t) => c.blit_scaled(t, r, 255, 10),
                None => {
                    c.fill_rounded_rect(r, 10, theme::shade(25));
                    c.draw_text_centered(&f.ui, r, "Can't open", theme::text_dim());
                }
            }
            let name = fs::file_name(path);
            let name = if name.chars().count() > 22 { format!("{}\u{2026}", name.chars().take(21).collect::<String>()) } else { String::from(name) };
            c.draw_text_centered(&f.ui, Rect::new(r.x, r.bottom() + 4, r.w, 20), &name, if selected { theme::text() } else { theme::text_dim() });
            self.hits.push((r, Action::Picture(i)));
        }
        let n = self.picture_thumbs.len() as i32;
        y += (n + cols - 1) / cols * (th + 34);
        let msg = if scanning {
            "Looking for pictures\u{2026}"
        } else if n == 0 {
            "No pictures yet. Copy PNG, JPEG or BMP files into /pictures, or attach a disk with your pictures in \
             VirtualBox (see Getting Started), then reopen this page. You can also right-click a picture in Files \
             and choose \u{201c}Set as Wallpaper\u{201d}."
        } else {
            "Pictures from /pictures and other attached disks. Right-click any picture in Files to use it too."
        };
        let bottom = self.note(c, x + 4, y + 4, w - 8, msg);
        self.extend(bottom + 16);
        bottom + 18
    }

    fn page_sound(&mut self, c: &mut Canvas, x: i32, mut y: i32, w: i32) {
        y = self.page_title(c, x, y, "Sound");
        let present = audio::is_present();
        let card = Rect::new(x, y, w, ROW_H * 4);
        self.card(c, card);
        let r = self.row(c, card, 0, "Output device", None, false);
        self.value(c, r, &audio::device_name());
        let r = self.row(c, card, 1, "Volume", None, false);
        let label = format!("{}%", self.cfg.volume);
        self.slider(c, r, Slider::Volume, &label);
        let r = self.row(c, card, 2, "Mute", None, false);
        self.toggle(c, r, Toggle::Muted);
        let r = self.row(c, card, 3, "System sounds", Some("Startup chime and alerts"), true);
        self.toggle(c, r, Toggle::SystemSounds);
        y = card.bottom() + 18;
        if present {
            self.action_button(c, Rect::new(x, y, 170, 32), "Play test sound", ButtonStyle::Primary, Action::TestSound);
            self.note(c, x + 186, y + 21, w - 190, "Plays a tone on the left speaker, then the right.");
        } else {
            self.note(
                c,
                x + 4,
                y + 4,
                w - 8,
                "No supported sound card was found. MayOS drives the Intel AC'97 controller: in VirtualBox choose \
                 Audio Controller \"ICH AC97\"; with QEMU add -device AC97.",
            );
        }
    }

    fn page_network(&mut self, c: &mut Canvas, x: i32, mut y: i32, w: i32) {
        let f = fonts();
        y = self.page_title(c, x, y, "Network & Internet");
        let iphone = crate::iphone::status();
        if !iphone.is_empty() {
            y = self.note(c, x + 4, y + 4, w - 8, &format!("iPhone: {}", iphone)) + 12;
        }
        let Some(st) = crate::network::status() else {
            self.note(
                c,
                x + 4,
                y + 4,
                w - 8,
                "No supported network adapter was found. MayOS drives Intel (e1000, I210/I211, I217-I219, I225/I226) and \
                 Realtek (RTL8111/8168, RTL8125) wired adapters, and phones plugged in over USB with USB tethering or \
                 Personal Hotspot on. In VirtualBox choose \"Intel PRO/1000 MT Desktop\".",
            );
            return;
        };
        let connected = st.link_up && !st.ip.is_unspecified();
        // Status banner.
        let banner = Rect::new(x, y, w, 64);
        self.card(c, banner);
        let dot = if connected { rgb(0x2f, 0xa8, 0x5a) } else if st.link_up { rgb(0xf0, 0x8a, 0x24) } else { theme::DANGER };
        c.fill_circle(x + 30, y + 32, 7, dot);
        let headline = if connected {
            "Connected"
        } else if !st.link_up {
            "Cable unplugged"
        } else {
            match st.dhcp {
                crate::network::DhcpState::Failed => "No DHCP server answered",
                _ => "Getting an address\u{2026}",
            }
        };
        c.draw_text(&f.bold, x + 48, y + 28, headline, theme::text());
        let sub = format!("{} \u{00b7} {} Mb/s", st.adapter, st.speed_mbps);
        c.draw_text_clipped(&f.ui, x + 48, y + 46, &sub, w - 60, theme::text_dim());
        y = banner.bottom() + 16;

        // File sharing (the built-in web server).
        let sharing = self.cfg.file_sharing;
        let addrs = if sharing { crate::network::httpd::addresses() } else { Vec::new() };
        let card = Rect::new(x, y, w, ROW_H * (1 + addrs.len() as i32));
        self.card(c, card);
        let r = self.row(c, card, 0, "Share files with other computers", Some("Copy files in and out with any web browser"), addrs.is_empty());
        self.toggle(c, r, Toggle::FileSharing);
        for (i, a) in addrs.iter().enumerate() {
            let (url, how) = a.split_once("  ").unwrap_or((a.as_str(), ""));
            let label = if i == 0 { "Open in your browser" } else { "Or" };
            let how = how.trim().trim_start_matches('(').trim_end_matches(')');
            let r = self.row(c, card, i as i32 + 1, label, if how.is_empty() { None } else { Some(how) }, i + 1 == addrs.len());
            self.value(c, r, url);
        }
        y = card.bottom() + 8;
        if sharing {
            let (reqs, bytes) = crate::network::httpd::stats();
            let text = format!(
                "VirtualBox: Settings \u{2192} Network \u{2192} Adapter 1 \u{2192} Advanced \u{2192} Port Forwarding, add a rule with host port 8080 and guest port 80. \
                 With a bridged adapter use the IP address directly. {} requests served, {} received.",
                reqs,
                crate::fs::format_size(bytes)
            );
            y = self.note(c, x + 4, y + 10, w - 8, &text) + 8;
        } else {
            y += 10;
        }

        // Details.
        let dns = if st.dns.is_empty() { String::from("\u{2014}") } else { st.dns.iter().map(|d| d.to_string()).collect::<Vec<_>>().join(", ") };
        let rows: [(&str, String); 6] = [
            ("IP address", if st.ip.is_unspecified() { String::from("\u{2014}") } else { format!("{}/{}", st.ip, st.mask.prefix_len()) }),
            ("Gateway", if st.gateway.is_unspecified() { String::from("\u{2014}") } else { st.gateway.to_string() }),
            ("DNS servers", dns),
            ("MAC address", st.mac.to_string()),
            ("Configuration", match st.dhcp {
                crate::network::DhcpState::Disabled => String::from("Manual"),
                crate::network::DhcpState::Bound => String::from("Automatic (DHCP lease)"),
                _ => String::from("Automatic (DHCP)"),
            }),
            ("Traffic", format!("{} packets sent, {} received", st.tx_packets, st.rx_packets)),
        ];
        let card = Rect::new(x, y, w, ROW_H * rows.len() as i32);
        self.card(c, card);
        for (i, (k, v)) in rows.iter().enumerate() {
            let r = self.row(c, card, i as i32, k, None, i + 1 == rows.len());
            self.value(c, r, v);
        }
        y = card.bottom() + 18;

        // Configuration.
        let manual = !self.cfg.dhcp;
        let card = Rect::new(x, y, w, ROW_H * if manual { 5 } else { 1 });
        self.card(c, card);
        let r = self.row(c, card, 0, "Get settings automatically (DHCP)", None, !manual);
        self.toggle(c, r, Toggle::Dhcp);
        if manual {
            let labels = ["IP address", "Subnet mask", "Gateway", "DNS server"];
            for (i, label) in labels.iter().enumerate() {
                let r = self.row(c, card, i as i32 + 1, label, None, i == 3);
                let field = Rect::new(r.right() - 18 - 220, r.y + 10, 220, 32);
                let focused = self.focused_input == Some(i);
                self.inputs[i].render(c, field, focused);
                self.hits.push((field, Action::Input(i)));
            }
        }
        y = card.bottom() + 14;
        let mut bx = x;
        if manual {
            self.action_button(c, Rect::new(bx, y, 120, 32), "Apply", ButtonStyle::Primary, Action::ApplyStatic);
        } else {
            self.action_button(c, Rect::new(bx, y, 140, 32), "Renew address", ButtonStyle::Normal, Action::RenewDhcp);
        }
        bx += 150;
        self.action_button(c, Rect::new(bx, y, 130, 32), "Ping gateway", ButtonStyle::Normal, Action::PingGateway);
        bx += 140;
        self.action_button(c, Rect::new(bx, y, 130, 32), "Ping 1.1.1.1", ButtonStyle::Normal, Action::PingInternet);
        bx += 140;
        self.action_button(c, Rect::new(bx, y, 150, 32), "Look up a name", ButtonStyle::Normal, Action::LookupName);
        y += 50;
        if self.net_test_running {
            c.draw_text(&f.ui, x + 4, y, "Testing\u{2026}", theme::text_dim());
        } else if let Some((m, ok)) = NET_TEST.lock().clone() {
            c.draw_text_clipped(&f.ui, x + 4, y, &m, w - 8, if ok { rgb(0x2f, 0x8a, 0x4a) } else { theme::DANGER });
        }
    }

    fn page_input(&mut self, c: &mut Canvas, x: i32, mut y: i32, w: i32) {
        y = self.page_title(c, x, y, "Mouse & Keyboard");
        let card = Rect::new(x, y, w, ROW_H * 3);
        self.card(c, card);
        let r = self.row(c, card, 0, "Pointer speed", Some("Applies to PS/2 mice"), false);
        let l = format!("{}", self.cfg.pointer_speed);
        self.slider(c, r, Slider::PointerSpeed, &l);
        let r = self.row(c, card, 1, "Double-click speed", None, false);
        let l = format!("{} ms", self.cfg.double_click_ms);
        self.slider(c, r, Slider::DoubleClick, &l);
        let r = self.row(c, card, 2, "Natural scrolling", Some("Content follows your fingers"), true);
        self.toggle(c, r, Toggle::NaturalScroll);
        y = card.bottom() + 18;
        let card = Rect::new(x, y, w, ROW_H * 2);
        self.card(c, card);
        let r = self.row(c, card, 0, "Key repeat rate", None, false);
        let l = ["", "Slow", "Relaxed", "Normal", "Quick", "Fast"][self.cfg.key_repeat.clamp(1, 5) as usize].to_string();
        self.slider(c, r, Slider::KeyRepeat, &l);
        let r = self.row(c, card, 1, "Keyboard layout", None, true);
        self.value(c, r, "English (US)");
        y = card.bottom() + 16;
        self.note(c, x + 4, y + 4, w - 8, "Tip: Ctrl+Alt+T opens a terminal, Ctrl+Alt+E the file explorer and Ctrl+Alt+S Settings.");
    }

    fn page_datetime(&mut self, c: &mut Canvas, x: i32, mut y: i32, w: i32) {
        let f = fonts();
        y = self.page_title(c, x, y, "Date & Time");
        let card = Rect::new(x, y, w, 96);
        self.card(c, card);
        let t = settings::local_time();
        let time = super::wm::format_clock(&self.cfg);
        c.draw_text(&f.large, x + 22, y + 44, &time, theme::text());
        let date = format!("{:04}-{:02}-{:02}", t.year, t.month, t.day);
        c.draw_text(&f.ui, x + 24, y + 74, &date, theme::text_dim());
        y = card.bottom() + 18;
        let card = Rect::new(x, y, w, ROW_H * 3);
        self.card(c, card);
        let r = self.row(c, card, 0, "Use 24-hour clock", None, false);
        self.toggle(c, r, Toggle::Clock24h);
        let r = self.row(c, card, 1, "Show seconds", None, false);
        self.toggle(c, r, Toggle::ShowSeconds);
        let r = self.row(c, card, 2, "Time zone", Some("Offset from the hardware clock (UTC)"), true);
        let off = self.cfg.tz_offset_min;
        let label = format!("UTC{}{:02}:{:02}", if off < 0 { '-' } else { '+' }, off.abs() / 60, off.abs() % 60);
        let plus = Rect::new(r.right() - 18 - 34, r.y + 10, 34, 32);
        let minus = Rect::new(plus.x - 150, r.y + 10, 34, 32);
        self.action_button(c, minus, "\u{2013}", ButtonStyle::Normal, Action::TzMinus);
        self.action_button(c, plus, "+", ButtonStyle::Normal, Action::TzPlus);
        c.draw_text_centered(&f.bold, Rect::new(minus.right(), r.y + 10, plus.x - minus.right(), 32), &label, theme::text());
    }

    fn page_storage(&mut self, c: &mut Canvas, x: i32, mut y: i32, w: i32) {
        let f = fonts();
        y = self.page_title(c, x, y, "Storage");
        self.disk_targets.clear();

        // Status of a running or finished disk setup.
        match crate::storage::setup_state() {
            crate::storage::SetupState::Running(m) => {
                y = self.note(c, x + 4, y + 4, w - 8, &format!("Setting up disk: {}", m)) + 8;
            }
            crate::storage::SetupState::Done(m) => {
                let r = Rect::new(x, y, w, 44);
                c.fill_rounded_rect(r, 10, rgb(0xe3, 0xf5, 0xe8));
                c.draw_text_clipped(&f.ui, x + 14, y + 27, &m, w - 28, rgb(0x1f, 0x6f, 0x3a));
                y += 58;
            }
            crate::storage::SetupState::Failed(m) => {
                let r = Rect::new(x, y, w, 44);
                c.fill_rounded_rect(r, 10, rgb(0xfd, 0xe7, 0xe8));
                c.draw_text_clipped(&f.ui, x + 14, y + 27, &m, w - 28, theme::DANGER);
                y += 58;
            }
            crate::storage::SetupState::Idle => {}
        }

        for m in crate::fs::mounts() {
            let card = Rect::new(x, y, w, 104);
            self.card(c, card);
            super::icons::draw(c, Icon::Drive, x + 16, y + 16, 40);
            let title = if m.point == "/" { String::from("MayOS Disk") } else { format!("{}  \u{2014}  {}", m.point, m.label) };
            c.draw_text(&f.bold, x + 70, y + 30, &title, theme::text());
            let sub = if m.persistent { String::from(m.backend) } else { format!("{} \u{2014} set up a disk below to keep your files", m.backend) };
            c.draw_text_clipped(&f.ui, x + 70, y + 48, &sub, w - 250, if m.persistent { theme::text_dim() } else { theme::DANGER });
            let bar = Rect::new(x + 18, y + 66, w - 36, 10);
            c.fill_rounded_rect(bar, 5, theme::separator());
            let total = m.stats.total_bytes().max(1);
            let used = total - m.stats.free_bytes();
            let uw = ((bar.w as u64 * used / total) as i32).max(10);
            c.fill_rounded_rect(Rect::new(bar.x, bar.y, uw, bar.h), 5, theme::accent());
            let text = format!("{} used \u{00b7} {} free \u{00b7} {} total", crate::fs::format_size(used), crate::fs::format_size(m.stats.free_bytes()), crate::fs::format_size(total));
            c.draw_text(&f.ui, x + 18, y + 94, &text, theme::text_dim());
            if m.point != "/" && m.persistent && !crate::fs::root_is_persistent() {
                let i = self.disk_targets.len();
                self.disk_targets.push((crate::storage::Target::Mounted(m.point.clone()), format!("{} (\u{201c}{}\u{201d})", m.point, m.label), false));
                self.action_button(c, Rect::new(card.right() - 184, y + 20, 168, 32), "Use for MayOS", ButtonStyle::Primary, Action::SetupDisk(i));
            }
            y = card.bottom() + 14;
        }
        for d in crate::storage::blank_disks() {
            let card = Rect::new(x, y, w, 70);
            self.card(c, card);
            super::icons::draw(c, Icon::Drive, x + 16, y + 14, 40);
            c.draw_text(&f.bold, x + 70, y + 30, &format!("{} \u{2014} {}", d.name, d.model), theme::text());
            c.draw_text(&f.ui, x + 70, y + 50, &format!("{} \u{00b7} not formatted", crate::fs::format_size(d.bytes)), theme::text_dim());
            let i = self.disk_targets.len();
            self.disk_targets.push((crate::storage::Target::Blank(d.name.clone()), format!("{} ({}, {})", d.name, d.model, crate::fs::format_size(d.bytes)), true));
            self.action_button(c, Rect::new(card.right() - 184, y + 19, 168, 32), "Set up for MayOS", ButtonStyle::Primary, Action::SetupDisk(i));
            y = card.bottom() + 14;
        }
        if !crate::fs::root_is_persistent() && self.disk_targets.is_empty() {
            y = self.note(
                c,
                x + 4,
                y + 4,
                w - 8,
                "To keep your files, add a hard disk to the virtual machine (VirtualBox: Settings > Storage > add a \
                 hard disk to the SATA or IDE controller), start MayOS again and set it up here.",
            ) + 8;
        }
        y += 6;
        let card = Rect::new(x, y, w, ROW_H);
        self.card(c, card);
        let r = self.row(c, card, 0, "Settings file", None, true);
        self.value(c, r, settings::PATH);
        y = card.bottom() + 16;
        self.action_button(c, Rect::new(x, y, 190, 32), "Reset all settings", ButtonStyle::Danger, Action::ResetSettings);
    }

    fn page_about(&mut self, c: &mut Canvas, x: i32, mut y: i32, w: i32) {
        let f = fonts();
        y = self.page_title(c, x, y, "About");
        let rows = super::about::system_info();
        let card = Rect::new(x, y, w, ROW_H * rows.len() as i32);
        self.card(c, card);
        for (i, (k, v)) in rows.iter().enumerate() {
            let r = self.row(c, card, i as i32, k, None, i + 1 == rows.len());
            self.value(c, r, v);
        }
        y = card.bottom() + 16;
        self.note(c, x + 4, y + 4, w - 8, "MayOS is written from scratch in Rust: kernel, drivers, file system, network stack, audio and desktop.");
        let _ = &f;
    }

    fn apply_action(&mut self, a: Action, ctx: &mut Ctx) {
        match a {
            Action::Page(p) => self.set_page(p),
            Action::Toggle(t) => {
                let from = self.knob_pos(t);
                let v = !toggle_value(&self.cfg, t);
                settings::update(|s| set_toggle(s, t, v));
                self.cfg = settings::get();
                self.knobs.retain(|k| k.0 != t);
                if self.cfg.animations || t == Toggle::Animations {
                    self.knobs.push((t, from, uptime_ms()));
                }
                if t == Toggle::Dhcp && v {
                    crate::network::use_dhcp();
                }
            }
            Action::Resolution(w, h) => {
                self.dropdown = None;
                if (w, h) != super::display_mode() {
                    ctx.commands.push(Command::SetResolution(w, h));
                }
            }
            Action::OpenResolutions => {}
            Action::Theme(dark) => settings::update(|s| s.dark = dark),
            Action::Wallpaper(i) => settings::update(|s| {
                s.wallpaper = i;
                s.wallpaper_image.clear();
            }),
            Action::Picture(i) => {
                if let Some((p, _)) = self.picture_thumbs.get(i) {
                    let p = p.clone();
                    settings::update(|s| s.wallpaper_image = p);
                }
            }
            Action::Accent(i) => settings::update(|s| s.accent = i),
            Action::DockPosition(p) => settings::update(|s| s.dock_position = p),
            Action::DockSize(z) => settings::update(|s| s.dock_size = z),
            Action::TzMinus => settings::update(|s| s.tz_offset_min = (s.tz_offset_min - 30).max(-12 * 60)),
            Action::TzPlus => settings::update(|s| s.tz_offset_min = (s.tz_offset_min + 30).min(14 * 60)),
            Action::TestSound => audio::play_system(SystemSound::Test),
            Action::RenewDhcp => {
                crate::network::use_dhcp();
                self.flash(String::from("Requesting a new address\u{2026}"), true);
            }
            Action::ApplyStatic => {
                let vals: Vec<String> = self.inputs.iter().map(|i| i.text.trim().to_string()).collect();
                let bad = vals.iter().take(3).position(|v| net::Ipv4::parse(v).is_none());
                match bad {
                    Some(i) => self.flash(format!("\u{201c}{}\u{201d} is not a valid address", vals[i]), false),
                    None => {
                        settings::update(|s| {
                            s.static_ip = vals[0].clone();
                            s.static_mask = vals[1].clone();
                            s.static_gateway = vals[2].clone();
                            s.static_dns = vals[3].clone();
                        });
                        let cfg = settings::get();
                        settings::apply_network(&cfg);
                        self.flash(String::from("Network settings applied"), true);
                    }
                }
            }
            Action::PingGateway | Action::PingInternet | Action::LookupName => {
                if !self.net_test_running {
                    *NET_TEST.lock() = None;
                    self.net_test_running = true;
                    let kind = match a {
                        Action::PingGateway => 0,
                        Action::PingInternet => 1,
                        _ => 2,
                    };
                    crate::proc::sched::spawn_kernel("net-test", net_test_thread, kind);
                }
            }
            Action::Input(i) => {
                self.focused_input = Some(i);
            }
            Action::ResetSettings => {
                settings::update(|s| *s = Settings::default());
                self.cfg = settings::get();
                self.flash(String::from("All settings were reset to their defaults"), true);
            }
            Action::SetupDisk(i) => {
                if let Some((target, desc, erases)) = self.disk_targets.get(i).cloned() {
                    self.pending_setup = Some(target);
                    let msg = if erases {
                        format!("Erase {} and install MayOS's files on it?", desc)
                    } else {
                        format!("Copy MayOS's files to {} and use it as the main disk? Existing files are kept.", desc)
                    };
                    ctx.open_child(Box::new(super::dialog::Dialog::confirm(
                        "Set Up Disk",
                        &msg,
                        if erases { "Erase and Set Up" } else { "Set Up" },
                        erases,
                        TAG_SETUP_DISK,
                        ctx.window,
                    )));
                }
            }
            Action::Slider(_) => {}
        }
        ctx.redraw();
    }

    fn slider_to(&mut self, sl: Slider, track: Rect, x: i32) {
        let (lo, hi) = slider_range(sl);
        let tx = track.x + 12;
        let tw = track.w - 24;
        let v = lo + ((x - tx).clamp(0, tw) * (hi - lo) + tw / 2) / tw.max(1);
        if v != slider_value(&self.cfg, sl) {
            settings::update_live(|s| set_slider(s, sl, v));
            self.cfg = settings::get();
        }
    }
}

impl App for SettingsApp {
    fn title(&self) -> String {
        String::from("Settings")
    }
    fn icon(&self) -> Icon {
        Icon::Settings
    }
    fn kind(&self) -> AppKind {
        AppKind::Settings
    }
    fn initial_size(&self) -> (i32, i32) {
        (880, 600)
    }
    fn min_size(&self) -> (i32, i32) {
        (700, 420)
    }

    fn render(&mut self, c: &mut Canvas, (w, h): (i32, i32), focused: bool) {
        self.size = (w, h);
        self.hits.clear();
        let f = fonts();
        // Sidebar.
        c.fill_rect(Rect::new(0, 0, SIDEBAR_W, h), theme::sidebar_bg());
        c.vline(SIDEBAR_W - 1, 0, h, theme::separator());
        // Header: the logo, the computer's name.
        super::icons::draw(c, Icon::MayOS, 20, 18, 36);
        c.draw_text(&f.bold, 66, 34, "MayOS", theme::text());
        c.draw_text(&f.ui, 66, 51, &self.cfg.hostname, theme::text_dim());
        let mut y = 76;
        for &(p, name, color) in PAGES {
            let r = Rect::new(10, y, SIDEBAR_W - 20, 38);
            if self.page == p {
                c.fill_rounded_rect(r, 9, if focused { with_alpha(theme::accent(), 40) } else { theme::shade(22) });
            } else if self.hovered(Action::Page(p)) {
                c.fill_rounded_rect(r, 9, theme::shade(12));
            }
            Self::sidebar_icon(c, p, color, r.x + 8, r.y + 6);
            let font = if self.page == p { &f.bold } else { &f.ui };
            c.draw_text(font, r.x + 44, r.y + 24, name, theme::text());
            self.hits.push((r, Action::Page(p)));
            y += 42;
        }

        // Content, sliding in when the page changes.
        let area = Rect::new(SIDEBAR_W, 0, w - SIDEBAR_W, h);
        c.fill_rect(area, theme::panel_bg());
        let slide = if self.page_since != 0 {
            let p = ((uptime_ms() - self.page_since) as i64 * 1024 / PAGE_MS as i64).min(1024);
            let e = 1024 - (1024 - p) * (1024 - p) / 1024;
            (18 * (1024 - e) / 1024) as i32
        } else {
            0
        };
        let old = c.push_clip(area);
        self.bottom.set(0);
        let cw = (w - SIDEBAR_W - 72).min(760);
        let (x, cy) = (SIDEBAR_W + ((w - SIDEBAR_W - cw) / 2).max(28), 24 + slide - self.scroll);
        match self.page {
            Page::Display => self.page_display(c, x, cy, cw),
            Page::Personalization => self.page_personalization(c, x, cy, cw),
            Page::Sound => self.page_sound(c, x, cy, cw),
            Page::Network => self.page_network(c, x, cy, cw),
            Page::Input => self.page_input(c, x, cy, cw),
            Page::DateTime => self.page_datetime(c, x, cy, cw),
            Page::Storage => self.page_storage(c, x, cy, cw),
            Page::About => self.page_about(c, x, cy, cw),
        }
        if self.page == Page::Display {
            self.resolution_list(c);
        }
        // Scroll bar when the page is taller than the window.
        let content = self.bottom.get() + self.scroll + 24;
        if content > h {
            let max = content - h;
            if self.scroll > max {
                self.scroll = max;
            }
            super::widgets::draw_scrollbar(c, Rect::new(w - 11, 4, 9, h - 8), content, h, self.scroll);
        }
        // Toast message.
        if let Some((m, ok, _)) = &self.message {
            let tw = f.ui.measure(m) + 32;
            let r = Rect::new(SIDEBAR_W + (w - SIDEBAR_W - tw) / 2, 14, tw, 34);
            c.draw_shadow(r, 10, 12, with_alpha(0x000000, 60));
            c.fill_rounded_rect(r, 10, if *ok { rgb(0x2b, 0x2f, 0x38) } else { theme::DANGER });
            c.draw_text_centered(&f.ui, r, m, rgb(255, 255, 255));
        }
        c.restore_clip(old);
    }

    fn event(&mut self, ev: &AppEvent, ctx: &mut Ctx) {
        let hit = |s: &Self, x: i32, y: i32| s.hits.iter().rev().find(|(r, _)| r.contains(x, y)).map(|(_, a)| *a);
        match ev {
            AppEvent::MouseMove { x, y, .. } => {
                if let Some((sl, track)) = self.dragging {
                    self.slider_to(sl, track, *x);
                    ctx.redraw();
                    return;
                }
                let h = hit(self, *x, *y);
                if h != self.hover {
                    self.hover = h;
                    ctx.redraw();
                }
            }
            AppEvent::MouseLeave => {
                if self.hover.take().is_some() {
                    ctx.redraw();
                }
            }
            AppEvent::MouseDown { x, y, button: 0, .. } => {
                let found = self.hits.iter().rev().find(|(r, _)| r.contains(*x, *y)).copied();
                match found {
                    Some((r, Action::OpenResolutions)) => {
                        self.dropdown = if self.dropdown.is_some() { None } else { Some(r) };
                        ctx.redraw();
                    }
                    Some((r, Action::Slider(sl))) => {
                        self.dragging = Some((sl, r));
                        self.slider_to(sl, r, *x);
                        ctx.redraw();
                    }
                    Some((_, a)) => {
                        if !matches!(a, Action::Input(_)) {
                            self.focused_input = None;
                        }
                        self.apply_action(a, ctx);
                    }
                    None => {
                        self.focused_input = None;
                        ctx.redraw();
                    }
                }
            }
            AppEvent::MouseUp { .. } => {
                if let Some((sl, _)) = self.dragging.take() {
                    settings::save(&settings::get());
                    if sl == Slider::Volume {
                        audio::play_system(SystemSound::Click);
                    }
                    ctx.redraw();
                }
            }
            AppEvent::Wheel { delta, .. } => {
                let content = self.bottom.get() + self.scroll + 24;
                let max = (content - self.size.1).max(0);
                self.scroll = (self.scroll + delta * 40).clamp(0, max);
                self.dropdown = None;
                ctx.redraw();
            }
            AppEvent::Key(k) if k.pressed => {
                if let Some(i) = self.focused_input {
                    match k.key {
                        Key::Tab => self.focused_input = Some((i + 1) % 4),
                        Key::Enter => self.apply_action(Action::ApplyStatic, ctx),
                        Key::Escape => self.focused_input = None,
                        _ => {
                            self.inputs[i].key(k);
                        }
                    }
                    ctx.redraw();
                    return;
                }
                // Up/Down move between pages.
                let idx = PAGES.iter().position(|p| p.0 == self.page).unwrap_or(0);
                match k.key {
                    Key::Down => self.set_page(PAGES[(idx + 1) % PAGES.len()].0),
                    Key::Up => self.set_page(PAGES[(idx + PAGES.len() - 1) % PAGES.len()].0),
                    _ => return,
                }
                ctx.redraw();
            }
            AppEvent::Message(Msg::DialogResult { tag: TAG_SETUP_DISK, value }) => {
                if let (Some(_), Some(target)) = (value, self.pending_setup.take()) {
                    crate::storage::start_setup(target);
                }
                ctx.redraw();
            }
            AppEvent::Message(Msg::ResolutionResult(ok)) => {
                if *ok {
                    let mode = super::display_mode();
                    self.flash(format!("Resolution changed to {} \u{00d7} {}", mode.0, mode.1), true);
                } else {
                    self.flash(String::from("That resolution is not available on this display"), false);
                }
                ctx.redraw();
            }
            AppEvent::Focus(_) | AppEvent::Resized { .. } => ctx.redraw(),
            _ => {}
        }
    }

    fn tick(&mut self, ctx: &mut Ctx) {
        let now = uptime_ms();
        if let Some(scan) = &self.pictures {
            let mut sc = scan.lock();
            if !sc.found.is_empty() {
                for (p, img) in sc.found.drain(..) {
                    let thumb = img.map(|img| {
                        let mut s = Surface::new(img.width as i32, img.height as i32, 0);
                        for (d, px) in s.data.iter_mut().zip(img.pixels.iter()) {
                            *d = *px | 0xff00_0000;
                        }
                        s
                    });
                    self.picture_thumbs.push((p, thumb));
                }
                ctx.redraw();
            }
            if sc.done && !sc.reported {
                sc.reported = true;
                ctx.redraw();
            }
        }
        if settings::generation() != self.cfg_gen {
            self.cfg_gen = settings::generation();
            self.cfg = settings::get();
            ctx.redraw();
        }
        if self.animating() {
            ctx.redraw();
        } else {
            self.knobs.clear();
            if self.page_since != 0 {
                self.page_since = 0;
                ctx.redraw();
            }
        }
        if self.net_test_running && NET_TEST.lock().is_some() {
            self.net_test_running = false;
            ctx.redraw();
        }
        if let Some((_, _, until)) = &self.message
            && now > *until
        {
            self.message = None;
            ctx.redraw();
        }
        // Live pages refresh once a second.
        let refresh_ms = if matches!(crate::storage::setup_state(), crate::storage::SetupState::Running(_)) { 250 } else { 1000 };
        if matches!(self.page, Page::Network | Page::DateTime | Page::About | Page::Storage) && now - self.last_refresh >= refresh_ms {
            self.last_refresh = now;
            ctx.redraw();
        }
    }
}

pub fn boxed() -> Box<dyn App> {
    Box::new(SettingsApp::new())
}

pub struct PictureScan {
    /// (path, thumbnail) found since the app last collected them.
    found: Vec<(String, Option<image::Image>)>,
    done: bool,
    reported: bool,
}

const THUMB_W: u32 = 192;
const THUMB_H: u32 = 120;
const MAX_PICTURES: usize = 30;

fn start_picture_scan() -> Arc<Spin<PictureScan>> {
    let scan = Arc::new(Spin::new(PictureScan { found: Vec::new(), done: false, reported: false }));
    let raw = Arc::into_raw(scan.clone()) as usize;
    crate::proc::sched::spawn_kernel("picture-scan", picture_scan_thread, raw);
    scan
}

extern "C" fn picture_scan_thread(arg: usize) {
    let scan = unsafe { Arc::from_raw(arg as *const Spin<PictureScan>) };
    let mut dirs: Vec<String> = alloc::vec![String::from("/pictures"), String::from("/home")];
    for m in fs::mounts() {
        if m.point != "/" {
            dirs.push(m.point.clone());
            dirs.push(format!("{}/pictures", m.point));
            dirs.push(format!("{}/Pictures", m.point));
        }
    }
    let mut paths: Vec<String> = Vec::new();
    for d in dirs {
        let Ok(entries) = fs::read_dir(&d) else { continue };
        for e in entries {
            if !e.is_dir && super::imageview::is_image_name(&e.name) && paths.len() < MAX_PICTURES {
                paths.push(fs::join(&d, &e.name));
            }
        }
    }
    for p in paths {
        let thumb = fs::read_file(&p).ok().and_then(|d| image::decode(&d).ok()).map(|img| img.cover(THUMB_W, THUMB_H, 0xff20_2020));
        scan.lock().found.push((p, thumb));
    }
    scan.lock().done = true;
}
