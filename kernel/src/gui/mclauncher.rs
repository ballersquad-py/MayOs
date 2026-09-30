//! The Minecraft launcher, laid out like Mojang's: a sidebar with the
//! account, tabs (Play, Installations, Settings), a big picture and a bottom
//! bar with the installation and the Play button. The game is started by
//! the `minecraft` command (MayOS's Java launcher) in a terminal window
//! that shows its progress; signing in runs `minecraft --login` in the
//! background and opens the Microsoft page in Firefox.

use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use gfx::icons::Icon;
use gfx::{rgb, rgba, with_alpha, Canvas, Color, Rect};

use super::app::{App, AppEvent, AppKind, Ctx};
use super::theme::fonts;
use super::widgets::TextInput;
use crate::fs;
use crate::input::Key;
use crate::proc::process::{self, Console, Process};

const CONFIG: &str = "/config/minecraft.cfg";
const CLIENT_ID: &str = "/etc/minecraft-client-id";
const ACCOUNT: &str = "/home/.minecraft/mayos-account.json";

/// (name, detail, arguments for `minecraft`).
const VERSIONS: &[(&str, &str, &str)] = &[
    ("Latest release", "Newest version, Fabric + Sodium (fastest)", " --mods"),
    ("1.21.11", "Fabric + Sodium and friends", "1.21.11 --mods"),
    ("1.21.11 Vanilla", "No mods", "1.21.11 --vanilla"),
    ("1.8.9 PvP", "Forge + OptiFine", "1.8.9"),
];
const MEMORY: &[&str] = &["2G", "4G", "6G", "8G"];

// Mojang's launcher colours (it is dark whatever the desktop theme).
const BG: Color = rgb(0x26, 0x26, 0x26);
const SIDEBAR: Color = rgb(0x1b, 0x1b, 0x1b);
const BAR: Color = rgb(0x31, 0x31, 0x31);
const CARD: Color = rgb(0x31, 0x31, 0x31);
const CARD_HOVER: Color = rgb(0x3d, 0x3d, 0x3d);
const LINE: Color = rgb(0x44, 0x44, 0x44);
const TEXT: Color = rgb(0xff, 0xff, 0xff);
const DIM: Color = rgb(0xa8, 0xa8, 0xa8);
const GREEN: Color = rgb(0x3c, 0x85, 0x27);
const GREEN_HOVER: Color = rgb(0x2f, 0x6d, 0x1e);
const SIDEBAR_W: i32 = 232;
const HEADER_H: i32 = 86;
const BAR_H: i32 = 92;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Tab {
    Play,
    Installations,
    Settings,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Hit {
    Tab(Tab),
    Picker,
    PickVersion(usize),
    InstallPlay(usize),
    Play,
    SignIn,
    SignOut,
    Name,
    ClientId,
    Memory(usize),
    Fullscreen,
    OpenPage,
    CancelLogin,
    Folder,
    Backdrop,
}

struct Login {
    process: Option<Arc<Process>>,
    console: Arc<Console>,
    out: String,
    url: String,
    code: String,
    status: String,
    failed: bool,
    opened: bool,
}

pub struct McLauncher {
    tab: Tab,
    version: usize,
    memory: usize,
    fullscreen: bool,
    name: TextInput,
    client_id: TextInput,
    editing: Option<Hit>,
    picker_open: bool,
    hover: Option<Hit>,
    hits: Vec<(Rect, Hit)>,
    size: (i32, i32),
    login: Option<Login>,
    account: Option<String>,
}

fn installed() -> bool {
    fs::exists("/usr/bin/minecraft")
}

fn read_account() -> Option<String> {
    let text = fs::read_file(ACCOUNT).ok()?;
    let text = String::from_utf8_lossy(&text).into_owned();
    let at = text.find("\"name\":\"")? + 8;
    Some(String::from(&text[at..at + text[at..].find('"')?]))
}

fn hash(mut x: u32) -> u32 {
    x ^= x >> 16;
    x = x.wrapping_mul(0x7feb_352d);
    x ^= x >> 15;
    x = x.wrapping_mul(0x846c_a68b);
    x ^ (x >> 16)
}

/// A blocky landscape: sky, sun, hills of grass and dirt, a few trees.
fn scene(c: &mut Canvas, r: Rect) {
    c.fill_gradient_v(r, rgb(0x6f, 0xa8, 0xf0), rgb(0xb9, 0xdc, 0xfb));
    let b = 16;
    c.fill_rect(Rect::new(r.right() - 170, r.y + 40, b * 4, b * 4), rgb(0xff, 0xf3, 0xb0));
    // Clouds.
    for k in 0..5u32 {
        let cx = r.x + (hash(k + 7) % r.w.max(1) as u32) as i32;
        let cy = r.y + 30 + (hash(k + 11) % 90) as i32;
        let n = 3 + (hash(k) % 4) as i32;
        c.fill_rect(Rect::new(cx, cy, b * n, b), rgba(255, 255, 255, 220));
        c.fill_rect(Rect::new(cx + b, cy - b, b * (n - 2).max(1), b), rgba(255, 255, 255, 220));
    }
    let cols = r.w / b + 1;
    let base = r.y + r.h * 58 / 100;
    for i in 0..cols {
        let t = i as f32 * 0.21;
        let wave = libm::sinf(t) * 1.5 + libm::sinf(t * 0.37 + 1.3) * 2.5;
        let top = base + (wave as i32) * b;
        let x = r.x + i * b;
        let rows = (r.bottom() - top) / b + 1;
        for j in 0..rows {
            let y = top + j * b;
            let n = hash((i as u32) << 8 ^ j as u32) % 24;
            let col = if j == 0 {
                rgb(0x5c + n as u8 / 2, 0x9e + n as u8 / 3, 0x31)
            } else if j < 4 {
                rgb(0x86 + n as u8 / 2, 0x5f + n as u8 / 3, 0x3e)
            } else {
                rgb(0x7a + n as u8, 0x7a + n as u8, 0x7a + n as u8)
            };
            c.fill_rect(Rect::new(x, y, b, b), col);
        }
        if hash(i as u32 * 31) % 9 == 0 && i > 1 && i < cols - 2 {
            for j in 1..4 {
                c.fill_rect(Rect::new(x, top - j * b, b, b), rgb(0x6b, 0x4f, 0x2c));
            }
            for (dx, dy) in [(-1, 4), (0, 4), (1, 4), (-1, 5), (0, 5), (1, 5), (0, 6), (-2, 4), (2, 4)] {
                let g = 0x3a + (hash((i * 7 + dx + dy) as u32) % 20) as u8;
                c.fill_rect(Rect::new(x + dx * b, top - dy * b, b, b), rgb(0x2d, g + 0x30, 0x1e));
            }
        }
    }
}

impl McLauncher {
    pub fn new() -> McLauncher {
        let id = fs::read_file(CLIENT_ID).map(|d| String::from(String::from_utf8_lossy(&d).trim())).unwrap_or_default();
        let mut l = McLauncher {
            tab: Tab::Play,
            version: 0,
            memory: 1,
            fullscreen: false,
            name: TextInput::new("Player"),
            client_id: TextInput::new(&id),
            editing: None,
            picker_open: false,
            hover: None,
            hits: Vec::new(),
            size: (1000, 620),
            login: None,
            account: read_account(),
        };
        if let Ok(d) = fs::read_file(CONFIG) {
            for line in String::from_utf8_lossy(&d).lines() {
                match line.split_once('=') {
                    Some(("installation", v)) => l.version = v.parse().unwrap_or(0).min(VERSIONS.len() - 1),
                    Some(("memory", v)) => l.memory = v.parse().unwrap_or(1).min(MEMORY.len() - 1),
                    Some(("fullscreen", v)) => l.fullscreen = v == "1",
                    Some(("name", v)) if !v.is_empty() => l.name = TextInput::new(v),
                    _ => {}
                }
            }
        }
        l
    }

    fn save(&self) {
        let _ = fs::create_dir("/config");
        let text = format!("installation={}\nmemory={}\nfullscreen={}\nname={}\n", self.version, self.memory, self.fullscreen as u8, self.name.text);
        let _ = fs::write_file(CONFIG, text.as_bytes());
        let _ = fs::create_dir("/etc");
        let _ = fs::write_file("/etc/minecraft-user", self.name.text.as_bytes());
        let id = self.client_id.text.trim();
        if id.is_empty() {
            let _ = fs::remove(CLIENT_ID);
        } else {
            let _ = fs::write_file(CLIENT_ID, id.as_bytes());
        }
    }

    fn play(&mut self, i: usize, ctx: &mut Ctx) {
        self.version = i;
        self.save();
        let cmd = if !installed() {
            String::from("pkg install minecraft")
        } else {
            let mut c = format!("minecraft {} --memory {}", VERSIONS[i].2.trim(), MEMORY[self.memory]);
            if self.fullscreen {
                c.push_str(" --fullscreen");
            }
            c
        };
        ctx.open(alloc::boxed::Box::new(super::terminal::Terminal::with_command("/home", &cmd)));
    }

    fn start_login(&mut self) {
        self.save();
        let console = Console::new();
        let mut l = Login {
            process: None,
            console: console.clone(),
            out: String::new(),
            url: String::new(),
            code: String::new(),
            status: String::from("Contacting Microsoft\u{2026}"),
            failed: false,
            opened: false,
        };
        if !installed() {
            l.status = String::from("Install Minecraft first (Play \u{2192} Install).");
            l.failed = true;
        } else if self.client_id.text.trim().is_empty() {
            l.status = String::from("Add a Microsoft app ID in Settings first.");
            l.failed = true;
        } else {
            match super::shell::find_program("/home", "minecraft").ok_or(String::from("minecraft is not installed")).and_then(|p| process::spawn(&p, "--login", "/home", console)) {
                Ok(p) => l.process = Some(p),
                Err(e) => {
                    l.status = e;
                    l.failed = true;
                }
            }
        }
        self.login = Some(l);
    }

    fn open_page(l: &mut Login) {
        let url = format!("{}?otc={}", l.url, l.code);
        if let Err(e) = super::detached::run("/home", &format!("firefox {}", url)) {
            l.status = format!("Couldn't open Firefox ({}): open {} on any device", e, l.url);
        }
        l.opened = true;
    }

    fn poll_login(&mut self) -> bool {
        let Some(l) = self.login.as_mut() else { return false };
        let mut changed = false;
        let out = l.console.take_output();
        if !out.is_empty() {
            l.out.push_str(&String::from_utf8_lossy(&out));
            changed = true;
        }
        let lines: Vec<String> = l.out.lines().map(String::from).collect();
        for line in &lines {
            if let Some(rest) = line.strip_prefix("MAYOS-LOGIN ") {
                let mut it = rest.split_whitespace();
                if let (Some(u), Some(c)) = (it.next(), it.next())
                    && l.code != c
                {
                    l.url = String::from(u);
                    l.code = String::from(c);
                    l.status = String::from("Waiting for you to sign in\u{2026}");
                    if !l.opened {
                        Self::open_page(l);
                    }
                }
            } else if let Some(name) = line.strip_prefix("MAYOS-LOGIN-OK ") {
                self.account = Some(String::from(name.trim()));
                self.login = None;
                return true;
            } else if let Some(e) = line.strip_prefix("Microsoft sign-in: ") {
                l.status = String::from(e);
                l.failed = true;
            }
        }
        if let Some(p) = &l.process
            && let Some(code) = p.has_exited()
        {
            process::reap(p.pid);
            l.process = None;
            if !l.failed {
                l.failed = true;
                l.status = format!("Sign-in stopped (code {})", code);
            }
            changed = true;
        }
        changed
    }

    fn push(&mut self, r: Rect, h: Hit) -> bool {
        self.hits.push((r, h));
        self.hover == Some(h)
    }

    fn render_sidebar(&mut self, c: &mut Canvas, h: i32) {
        let f = fonts();
        c.fill_rect(Rect::new(0, 0, SIDEBAR_W, h), SIDEBAR);
        // Account.
        let acct = Rect::new(12, 14, SIDEBAR_W - 24, 56);
        super::icons::draw(c, Icon::Minecraft, acct.x + 6, acct.y + 10, 36);
        let (who, kind) = match &self.account {
            Some(n) => (n.clone(), "Microsoft account"),
            None => (self.name.text.clone(), "Offline"),
        };
        c.draw_text_clipped(&f.heavy, acct.x + 52, acct.y + 26, &who, acct.w - 56, TEXT);
        c.draw_text(&f.ui, acct.x + 52, acct.y + 45, kind, DIM);
        c.fill_rect(Rect::new(12, 82, SIDEBAR_W - 24, 1), LINE);
        // The game.
        let g = Rect::new(8, 94, SIDEBAR_W - 16, 52);
        c.fill_rounded_rect(g, 8, rgb(0x33, 0x33, 0x33));
        c.fill_rounded_rect(Rect::new(g.x + 2, g.y + 12, 4, g.h - 24), 2, GREEN);
        super::icons::draw(c, Icon::Minecraft, g.x + 12, g.y + 10, 32);
        c.draw_text(&f.small_bold, g.x + 54, g.y + 22, "MINECRAFT:", TEXT);
        c.draw_text(&f.small_bold, g.x + 54, g.y + 38, "JAVA EDITION", TEXT);
        // Sign in / out at the bottom.
        let b = Rect::new(12, h - 56, SIDEBAR_W - 24, 40);
        if self.account.is_some() {
            let hov = self.push(b, Hit::SignOut);
            c.fill_rounded_rect(b, 8, if hov { CARD_HOVER } else { CARD });
            c.draw_text_centered(&f.bold, b, "Sign out", TEXT);
        } else {
            let hov = self.push(b, Hit::SignIn);
            c.fill_rounded_rect(b, 8, if hov { GREEN_HOVER } else { GREEN });
            c.draw_text_centered(&f.bold, b, "Sign in with Microsoft", TEXT);
        }
    }

    fn render_header(&mut self, c: &mut Canvas, x: i32, w: i32) {
        let f = fonts();
        c.fill_rect(Rect::new(x, 0, w - x, HEADER_H), BG);
        c.draw_text(&f.large, x + 28, 38, "MINECRAFT: JAVA EDITION", TEXT);
        let mut tx = x + 28;
        for (t, label) in [(Tab::Play, "Play"), (Tab::Installations, "Installations"), (Tab::Settings, "Settings")] {
            let tw = f.bold.measure(label);
            let r = Rect::new(tx - 4, 52, tw + 8, 34);
            let hov = self.push(r, Hit::Tab(t));
            let sel = self.tab == t;
            c.draw_text(&f.bold, tx, 74, label, if sel || hov { TEXT } else { DIM });
            if sel {
                c.fill_rounded_rect(Rect::new(tx, HEADER_H - 4, tw, 4), 2, GREEN);
            }
            tx += tw + 32;
        }
        c.fill_rect(Rect::new(x, HEADER_H - 1, w - x, 1), LINE);
    }

    fn render_play(&mut self, c: &mut Canvas, x: i32, w: i32, h: i32) {
        let f = fonts();
        let hero = Rect::new(x, HEADER_H, w - x, h - HEADER_H - BAR_H);
        let old = c.push_clip(hero);
        scene(c, hero);
        let title = Rect::new(hero.x, hero.y + hero.h / 5, hero.w, 48);
        c.draw_text_centered(&f.large, Rect::new(title.x + 3, title.y + 3, title.w, title.h), "MINECRAFT", rgba(0, 0, 0, 120));
        c.draw_text_centered(&f.large, title, "MINECRAFT", TEXT);
        c.draw_text_centered(&f.bold, Rect::new(hero.x, title.bottom() + 2, hero.w, 22), "JAVA EDITION", rgba(255, 255, 255, 230));
        c.restore_clip(old);

        // Bottom bar.
        let bar = Rect::new(x, h - BAR_H, w - x, BAR_H);
        c.fill_rect(bar, BAR);
        let picker = Rect::new(x + 24, bar.y + 18, 260, 56);
        let hov = self.push(picker, Hit::Picker);
        c.fill_rounded_rect(picker, 8, if hov || self.picker_open { CARD_HOVER } else { rgb(0x2a, 0x2a, 0x2a) });
        super::icons::draw(c, Icon::Minecraft, picker.x + 12, picker.y + 12, 32);
        c.draw_text_clipped(&f.bold, picker.x + 56, picker.y + 25, VERSIONS[self.version].0, picker.w - 90, TEXT);
        c.draw_text_clipped(&f.ui, picker.x + 56, picker.y + 44, VERSIONS[self.version].1, picker.w - 90, DIM);
        let (cx, cy) = (picker.right() - 22, picker.y + 26);
        for k in 0..5 {
            c.fill_rect(Rect::new(cx - 5 + k, cy + 4 - k, 11 - 2 * k, 1), DIM);
        }
        let pw = 260.min((bar.w - 600).max(200));
        let play = Rect::new((bar.x + (bar.w - pw) / 2).max(picker.right() + 24), bar.y + 16, pw, 60);
        let hov = self.push(play, Hit::Play);
        c.fill_rounded_rect(play, 8, if hov { GREEN_HOVER } else { GREEN });
        c.draw_text_centered(&f.large, play, if installed() { "PLAY" } else { "INSTALL" }, TEXT);
        let who = match &self.account {
            Some(n) => format!("{}", n),
            None => format!("{} (offline)", self.name.text),
        };
        let ww = f.ui.measure(&who).min(220);
        c.draw_text(&f.ui, bar.right() - 24 - ww, bar.y + 42, "Playing as", DIM);
        c.draw_text_clipped(&f.heavy, bar.right() - 24 - ww, bar.y + 62, &who, 220, TEXT);

        if self.picker_open {
            let ih = 56;
            let list = Rect::new(picker.x, picker.y - 8 - ih * VERSIONS.len() as i32 - 8, 320, ih * VERSIONS.len() as i32 + 8);
            self.hits.push((Rect::new(0, 0, self.size.0, self.size.1), Hit::Backdrop));
            c.draw_shadow(list, 10, 16, with_alpha(0x000000, 120));
            c.fill_rounded_rect(list, 10, rgb(0x2a, 0x2a, 0x2a));
            for i in 0..VERSIONS.len() {
                let r = Rect::new(list.x + 4, list.y + 4 + i as i32 * ih, list.w - 8, ih);
                let hov = self.push(r, Hit::PickVersion(i));
                if hov || i == self.version {
                    c.fill_rounded_rect(r, 8, if hov { CARD_HOVER } else { rgb(0x35, 0x35, 0x35) });
                }
                c.draw_text(&f.bold, r.x + 14, r.y + 24, VERSIONS[i].0, TEXT);
                c.draw_text(&f.ui, r.x + 14, r.y + 43, VERSIONS[i].1, DIM);
            }
        }
    }

    fn render_installations(&mut self, c: &mut Canvas, x: i32, w: i32, h: i32) {
        let f = fonts();
        c.fill_rect(Rect::new(x, HEADER_H, w - x, h - HEADER_H), BG);
        let mut y = HEADER_H + 24;
        for i in 0..VERSIONS.len() {
            let r = Rect::new(x + 28, y, w - x - 56, 64);
            c.fill_rect(Rect::new(r.x, r.bottom(), r.w, 1), LINE);
            super::icons::draw(c, Icon::Minecraft, r.x + 8, r.y + 16, 32);
            c.draw_text(&f.bold, r.x + 56, r.y + 28, VERSIONS[i].0, TEXT);
            c.draw_text(&f.ui, r.x + 56, r.y + 48, VERSIONS[i].1, DIM);
            let b = Rect::new(r.right() - 110, r.y + 14, 100, 36);
            let hov = self.push(b, Hit::InstallPlay(i));
            c.fill_rounded_rect(b, 8, if hov { GREEN_HOVER } else { GREEN });
            c.draw_text_centered(&f.bold, b, if installed() { "Play" } else { "Install" }, TEXT);
            y += 72;
        }
        let b = Rect::new(x + 28, y + 16, 220, 36);
        let hov = self.push(b, Hit::Folder);
        c.fill_rounded_rect(b, 8, if hov { CARD_HOVER } else { CARD });
        c.draw_text_centered(&f.bold, b, "Open game folder", TEXT);
    }

    fn render_settings(&mut self, c: &mut Canvas, x: i32, w: i32, h: i32) {
        let f = fonts();
        c.fill_rect(Rect::new(x, HEADER_H, w - x, h - HEADER_H), BG);
        let lx = x + 28;
        let mut y = HEADER_H + 30;
        let label = |c: &mut Canvas, y: i32, t: &str, d: &str| {
            c.draw_text(&f.bold, lx, y, t, TEXT);
            c.draw_text(&f.ui, lx, y + 20, d, DIM);
        };
        label(c, y, "Player name", "Used when you play offline");
        let r = Rect::new(w - 28 - 300, y - 18, 300, 36);
        self.push(r, Hit::Name);
        self.name.render(c, r, self.editing == Some(Hit::Name));
        y += 64;
        label(c, y, "Memory", "How much RAM the game may use");
        for i in 0..MEMORY.len() {
            let r = Rect::new(w - 28 - 300 + i as i32 * 76, y - 18, 72, 36);
            let hov = self.push(r, Hit::Memory(i));
            let sel = self.memory == i;
            c.fill_rounded_rect(r, 8, if sel { GREEN } else if hov { CARD_HOVER } else { CARD });
            c.draw_text_centered(&f.bold, r, MEMORY[i], TEXT);
        }
        y += 64;
        label(c, y, "Fullscreen", "Start 1.8.9 and older versions fullscreen");
        let t = Rect::new(w - 28 - 52, y - 14, 52, 28);
        let hov = self.push(t, Hit::Fullscreen);
        c.fill_rounded_rect(t, 14, if self.fullscreen { GREEN } else if hov { rgb(0x55, 0x55, 0x55) } else { rgb(0x48, 0x48, 0x48) });
        c.fill_circle(if self.fullscreen { t.right() - 14 } else { t.x + 14 }, t.y + 14, 10, TEXT);
        y += 64;
        label(c, y, "Microsoft app ID", "Needed to sign in: an Azure app approved for Minecraft");
        let r = Rect::new(w - 28 - 300, y - 18, 300, 36);
        self.push(r, Hit::ClientId);
        self.client_id.render(c, r, self.editing == Some(Hit::ClientId));
        y += 60;
        for line in [
            "Microsoft only lets approved apps sign in to Minecraft. Put your app's ID here (it is saved in",
            "/etc/minecraft-client-id). Without one you can still play offline worlds with the player name.",
        ] {
            c.draw_text(&f.ui, lx, y, line, DIM);
            y += 20;
        }
    }

    fn render_login(&mut self, c: &mut Canvas, w: i32, h: i32) {
        let f = fonts();
        let Some(l) = &self.login else { return };
        let (code, url, status, failed) = (l.code.clone(), l.url.clone(), l.status.clone(), l.failed);
        self.hits.push((Rect::new(0, 0, w, h), Hit::Backdrop));
        c.fill_rect(Rect::new(0, 0, w, h), rgba(0, 0, 0, 150));
        let r = Rect::new((w - 460) / 2, (h - 300) / 2, 460, 300);
        c.draw_shadow(r, 12, 20, with_alpha(0x000000, 140));
        c.fill_rounded_rect(r, 12, rgb(0x2a, 0x2a, 0x2a));
        c.draw_text_centered(&f.large, Rect::new(r.x, r.y + 20, r.w, 36), "Sign in with Microsoft", TEXT);
        if code.is_empty() {
            c.draw_text_centered(&f.ui, Rect::new(r.x + 20, r.y + 110, r.w - 40, 24), &status, if failed { rgb(0xff, 0x8a, 0x80) } else { DIM });
        } else {
            c.draw_text_centered(&f.ui, Rect::new(r.x, r.y + 70, r.w, 20), "Firefox opened the sign-in page. If it asks, enter:", DIM);
            let cr = Rect::new(r.x + 60, r.y + 100, r.w - 120, 60);
            c.fill_rounded_rect(cr, 8, rgb(0x1b, 0x1b, 0x1b));
            c.draw_text_centered(&f.large, cr, &code, TEXT);
            c.draw_text_centered(&f.ui, Rect::new(r.x, r.y + 172, r.w, 20), &format!("at {}", url), DIM);
            c.draw_text_centered(&f.ui, Rect::new(r.x + 20, r.y + 196, r.w - 40, 20), &status, if failed { rgb(0xff, 0x8a, 0x80) } else { DIM });
            let b = Rect::new(r.x + 30, r.bottom() - 60, 190, 40);
            let hov = self.push(b, Hit::OpenPage);
            c.fill_rounded_rect(b, 8, if hov { GREEN_HOVER } else { GREEN });
            c.draw_text_centered(&f.bold, b, "Open page again", TEXT);
        }
        let b = if code.is_empty() { Rect::new(r.x + (r.w - 190) / 2, r.bottom() - 60, 190, 40) } else { Rect::new(r.right() - 220, r.bottom() - 60, 190, 40) };
        let hov = self.push(b, Hit::CancelLogin);
        c.fill_rounded_rect(b, 8, if hov { CARD_HOVER } else { rgb(0x3a, 0x3a, 0x3a) });
        c.draw_text_centered(&f.bold, b, if failed { "Close" } else { "Cancel" }, TEXT);
    }
}

impl App for McLauncher {
    fn title(&self) -> String {
        String::from("Minecraft Launcher")
    }
    fn icon(&self) -> Icon {
        Icon::Minecraft
    }
    fn kind(&self) -> AppKind {
        AppKind::Minecraft
    }
    fn initial_size(&self) -> (i32, i32) {
        (1000, 620)
    }
    fn min_size(&self) -> (i32, i32) {
        (900, 560)
    }

    fn render(&mut self, c: &mut Canvas, (w, h): (i32, i32), _focused: bool) {
        self.size = (w, h);
        self.hits.clear();
        self.render_sidebar(c, h);
        self.render_header(c, SIDEBAR_W, w);
        match self.tab {
            Tab::Play => self.render_play(c, SIDEBAR_W, w, h),
            Tab::Installations => self.render_installations(c, SIDEBAR_W, w, h),
            Tab::Settings => self.render_settings(c, SIDEBAR_W, w, h),
        }
        self.render_login(c, w, h);
    }

    fn event(&mut self, ev: &AppEvent, ctx: &mut Ctx) {
        let find = |s: &Self, x: i32, y: i32| s.hits.iter().rev().find(|(r, _)| r.contains(x, y)).map(|(_, h)| *h);
        match *ev {
            AppEvent::MouseMove { x, y, .. } => {
                let h = find(self, x, y);
                if h != self.hover {
                    self.hover = h;
                    ctx.redraw();
                }
            }
            AppEvent::MouseDown { x, y, button: 0, .. } => {
                let hit = find(self, x, y);
                self.editing = match hit {
                    Some(Hit::Name) if self.account.is_none() => Some(Hit::Name),
                    Some(Hit::ClientId) => Some(Hit::ClientId),
                    _ => None,
                };
                let picker_was = self.picker_open;
                self.picker_open = false;
                match hit {
                    Some(Hit::Tab(t)) => self.tab = t,
                    Some(Hit::Picker) => self.picker_open = !picker_was,
                    Some(Hit::PickVersion(i)) => self.version = i,
                    Some(Hit::InstallPlay(i)) => self.play(i, ctx),
                    Some(Hit::Play) => self.play(self.version, ctx),
                    Some(Hit::SignIn) => self.start_login(),
                    Some(Hit::SignOut) => {
                        let _ = fs::remove(ACCOUNT);
                        self.account = None;
                    }
                    Some(Hit::Memory(i)) => self.memory = i,
                    Some(Hit::Fullscreen) => self.fullscreen = !self.fullscreen,
                    Some(Hit::OpenPage) => {
                        if let Some(l) = self.login.as_mut() {
                            Self::open_page(l);
                        }
                    }
                    Some(Hit::CancelLogin) => {
                        if let Some(l) = self.login.take()
                            && let Some(p) = l.process
                        {
                            process::kill(&p);
                            process::reap(p.pid);
                        }
                    }
                    Some(Hit::Folder) => {
                        let _ = fs::create_dir("/home/.minecraft");
                        ctx.open(alloc::boxed::Box::new(super::explorer::Explorer::new("/home/.minecraft")));
                    }
                    _ => {}
                }
                self.save();
                ctx.redraw();
            }
            AppEvent::Key(ref k) if k.pressed && k.key == Key::Escape => {
                self.picker_open = false;
                self.editing = None;
                ctx.redraw();
            }
            AppEvent::Key(ref k) if self.editing.is_some() => {
                if k.pressed && k.key == Key::Enter {
                    self.editing = None;
                } else if self.editing == Some(Hit::Name) {
                    self.name.key(k);
                    self.name.text.retain(|ch| ch.is_ascii_alphanumeric() || ch == '_');
                    self.name.text.truncate(16);
                    self.name.caret = self.name.caret.min(self.name.text.len());
                } else {
                    self.client_id.key(k);
                    self.client_id.text.retain(|ch| ch.is_ascii_hexdigit() || ch == '-');
                    self.client_id.caret = self.client_id.caret.min(self.client_id.text.len());
                }
                self.save();
                ctx.redraw();
            }
            _ => {}
        }
    }

    fn tick(&mut self, ctx: &mut Ctx) {
        if self.poll_login() {
            ctx.redraw();
        }
    }
}
