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
use super::theme;
use super::widgets::TextInput;
use crate::fs;
use crate::input::Key;
use crate::proc::process::{self, Console, Process};

const CONFIG: &str = "/config/minecraft.cfg";
const CLIENT_ID: &str = "/etc/minecraft-client-id";
/// Used when /etc/minecraft-client-id is empty (same as Launcher.java).
const DEFAULT_CLIENT_ID: &str = "4828c89c-ee13-40fa-aa8a-36fb6ad65f60";
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
    panorama: Option<Art>,
    logo: Option<Art>,
    edition: Option<Art>,
    /// The panorama fitted to the picture area (rebuilt on resize).
    hero: Option<gfx::Surface>,
    art_checked: u64,
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

const ART_DIR: &str = "/home/.minecraft/mayos-art";

/// A picture from the game's own files (extracted by `minecraft --art`).
struct Art {
    w: i32,
    h: i32,
    px: Vec<u32>,
}

fn load_art(name: &str) -> Option<Art> {
    let data = fs::read_file(&format!("{}/{}", ART_DIR, name)).ok()?;
    let img = image::decode(&data).ok()?;
    Some(Art { w: img.width as i32, h: img.height as i32, px: img.pixels })
}

/// Box-filtered crop-and-scale of `src` (region `from`) to w x h, opaque.
fn scale_cover(src: &Art, w: i32, h: i32) -> gfx::Surface {
    // Crop to the destination's aspect ratio, centred.
    let (mut fw, mut fh) = (src.w, src.w * h / w.max(1));
    if fh > src.h {
        fh = src.h;
        fw = src.h * w / h.max(1);
    }
    let (fx, fy) = ((src.w - fw) / 2, (src.h - fh) / 2);
    let mut out = gfx::Surface::new(w, h, 0);
    for oy in 0..h {
        let y0 = fy + oy * fh / h;
        let y1 = (fy + (oy + 1) * fh / h).max(y0 + 1).min(src.h);
        for ox in 0..w {
            let x0 = fx + ox * fw / w;
            let x1 = (fx + (ox + 1) * fw / w).max(x0 + 1).min(src.w);
            let (mut r, mut g, mut b, mut n) = (0u32, 0u32, 0u32, 0u32);
            for y in y0..y1 {
                for x in x0..x1 {
                    let p = src.px[(y * src.w + x) as usize];
                    r += p >> 16 & 0xff;
                    g += p >> 8 & 0xff;
                    b += p & 0xff;
                    n += 1;
                }
            }
            let n = n.max(1);
            out.data[(oy * w + ox) as usize] = 0xff00_0000 | (r / n) << 16 | (g / n) << 8 | (b / n);
        }
    }
    out
}

/// Pixel art at a whole-number scale, with its transparency.
fn draw_pixels(c: &mut Canvas, a: &Art, x: i32, y: i32, k: i32) {
    for j in 0..a.h * k {
        for i in 0..a.w * k {
            let p = a.px[((j / k) * a.w + i / k) as usize];
            let al = p >> 24;
            if al != 0 {
                c.blend_pixel(x + i, y + j, p | 0xff00_0000, al);
            }
        }
    }
}

/// Mojang's launcher uses Noto Sans throughout.
struct LFonts {
    ui: &'static gfx::Font,
    bold: &'static gfx::Font,
    heavy: &'static gfx::Font,
    small_bold: &'static gfx::Font,
    large: &'static gfx::Font,
    pixel: &'static gfx::Font,
    pixel_big: &'static gfx::Font,
}

fn fonts() -> LFonts {
    let f = theme::fonts();
    LFonts { ui: &f.mc_ui, bold: &f.mc_bold, heavy: &f.mc_bold, small_bold: &f.mc_small, large: &f.mc_title, pixel: &f.mc_pixel, pixel_big: &f.mc_pixel_big }
}

/// A square text field (dark, 1 px border, green when editing).
fn text_field(c: &mut Canvas, r: Rect, text: &str, editing: bool) {
    let f = fonts();
    c.fill_rect(r, rgb(0x1b, 0x1b, 0x1b));
    let b = if editing { GREEN } else { LINE };
    c.fill_rect(Rect::new(r.x, r.y, r.w, 1), b);
    c.fill_rect(Rect::new(r.x, r.bottom() - 1, r.w, 1), b);
    c.fill_rect(Rect::new(r.x, r.y, 1, r.h), b);
    c.fill_rect(Rect::new(r.right() - 1, r.y, 1, r.h), b);
    let base = r.y + (r.h + f.ui.ascent - f.ui.descent) / 2;
    c.draw_text_clipped(f.ui, r.x + 10, base, text, r.w - 20, TEXT);
    if editing {
        let x = r.x + 10 + f.ui.measure(text).min(r.w - 22);
        c.fill_rect(Rect::new(x + 1, r.y + 8, 1, r.h - 16), TEXT);
    }
}

impl McLauncher {
    pub fn new() -> McLauncher {
        let id = fs::read_file(CLIENT_ID).map(|d| String::from(String::from_utf8_lossy(&d).trim())).unwrap_or_default();
        let id = if id.is_empty() { String::from(DEFAULT_CLIENT_ID) } else { id };
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
            panorama: None,
            logo: None,
            edition: None,
            hero: None,
            art_checked: 0,
        };
        l.reload_art();
        if l.panorama.is_none() && installed() {
            let _ = super::detached::run("/home", "minecraft --art");
        }
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

    fn reload_art(&mut self) {
        self.panorama = load_art("panorama.png");
        self.logo = load_art("logo.png").filter(|a| a.w > a.h * 3);
        self.edition = load_art("edition.png");
        self.hero = None;
    }

    fn save(&self) {
        let _ = fs::create_dir("/config");
        let text = format!("installation={}\nmemory={}\nfullscreen={}\nname={}\n", self.version, self.memory, self.fullscreen as u8, self.name.text);
        let _ = fs::write_file(CONFIG, text.as_bytes());
        let _ = fs::create_dir("/etc");
        let _ = fs::write_file("/etc/minecraft-user", self.name.text.as_bytes());
        let id = self.client_id.text.trim();
        if id.is_empty() || id == DEFAULT_CLIENT_ID {
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
        c.draw_text_clipped(f.heavy, acct.x + 52, acct.y + 26, &who, acct.w - 56, TEXT);
        c.draw_text(f.ui, acct.x + 52, acct.y + 45, kind, DIM);
        c.fill_rect(Rect::new(12, 82, SIDEBAR_W - 24, 1), LINE);
        // The game.
        let g = Rect::new(8, 94, SIDEBAR_W - 16, 52);
        c.fill_rect(g, rgb(0x33, 0x33, 0x33));
        c.fill_rect(Rect::new(g.x, g.y, 3, g.h), GREEN);
        super::icons::draw(c, Icon::Minecraft, g.x + 12, g.y + 10, 32);
        c.draw_text(f.small_bold, g.x + 54, g.y + 22, "MINECRAFT:", TEXT);
        c.draw_text(f.small_bold, g.x + 54, g.y + 38, "JAVA EDITION", TEXT);
        // Sign in / out at the bottom.
        let b = Rect::new(12, h - 56, SIDEBAR_W - 24, 40);
        if self.account.is_some() {
            let hov = self.push(b, Hit::SignOut);
            c.fill_rect(b, if hov { CARD_HOVER } else { CARD });
            c.draw_text_centered(f.bold, b, "Sign out", TEXT);
        } else {
            let hov = self.push(b, Hit::SignIn);
            c.fill_rect(b, if hov { GREEN_HOVER } else { GREEN });
            c.draw_text_centered(f.bold, b, "Sign in with Microsoft", TEXT);
        }
    }

    fn render_header(&mut self, c: &mut Canvas, x: i32, w: i32) {
        let f = fonts();
        c.fill_rect(Rect::new(x, 0, w - x, HEADER_H), BG);
        c.draw_text(f.large, x + 28, 38, "MINECRAFT: JAVA EDITION", TEXT);
        let mut tx = x + 28;
        for (t, label) in [(Tab::Play, "Play"), (Tab::Installations, "Installations"), (Tab::Settings, "Settings")] {
            let tw = f.bold.measure(label);
            let r = Rect::new(tx - 4, 52, tw + 8, 34);
            let hov = self.push(r, Hit::Tab(t));
            let sel = self.tab == t;
            c.draw_text(f.bold, tx, 74, label, if sel || hov { TEXT } else { DIM });
            if sel {
                c.fill_rect(Rect::new(tx, HEADER_H - 4, tw, 4), GREEN);
            }
            tx += tw + 32;
        }
        c.fill_rect(Rect::new(x, HEADER_H - 1, w - x, 1), LINE);
    }

    fn render_play(&mut self, c: &mut Canvas, x: i32, w: i32, h: i32) {
        let f = fonts();
        let hero = Rect::new(x, HEADER_H, w - x, h - HEADER_H - BAR_H);
        match &self.panorama {
            Some(p) => {
                if self.hero.as_ref().map(|s| (s.w, s.h)) != Some((hero.w, hero.h)) {
                    self.hero = Some(scale_cover(p, hero.w, hero.h));
                }
                if let Some(s) = &self.hero {
                    c.blit(s, hero.x, hero.y);
                }
                c.fill_gradient_v(Rect::new(hero.x, hero.bottom() - 120, hero.w, 120), rgba(0, 0, 0, 0), rgba(0, 0, 0, 110));
            }
            None => c.fill_gradient_v(hero, rgb(0x2b, 0x2b, 0x2b), rgb(0x1f, 0x1f, 0x1f)),
        }
        match &self.logo {
            Some(l) => {
                let k = ((hero.w * 55 / 100) / l.w.max(1)).clamp(1, 4);
                let (lx, ly) = (hero.x + (hero.w - l.w * k) / 2, hero.y + hero.h / 6);
                draw_pixels(c, l, lx, ly, k);
                if let Some(e) = &self.edition {
                    let ke = k.max(1);
                    draw_pixels(c, e, hero.x + (hero.w - e.w * ke) / 2, ly + l.h * k - e.h * ke / 2, ke);
                }
            }
            None => {
                let title = Rect::new(hero.x, hero.y + hero.h / 5, hero.w, 48);
                c.draw_text_centered(f.pixel_big, Rect::new(title.x + 4, title.y + 4, title.w, title.h), "MINECRAFT", rgba(0, 0, 0, 110));
                c.draw_text_centered(f.pixel_big, title, "MINECRAFT", TEXT);
                c.draw_text_centered(f.bold, Rect::new(hero.x, title.bottom() + 2, hero.w, 22), "JAVA EDITION", rgba(255, 255, 255, 230));
            }
        }

        // Bottom bar.
        let bar = Rect::new(x, h - BAR_H, w - x, BAR_H);
        c.fill_rect(bar, BAR);
        let picker = Rect::new(x + 24, bar.y + 18, 260, 56);
        let hov = self.push(picker, Hit::Picker);
        c.fill_rect(picker, if hov || self.picker_open { CARD_HOVER } else { rgb(0x2a, 0x2a, 0x2a) });
        super::icons::draw(c, Icon::Minecraft, picker.x + 12, picker.y + 12, 32);
        c.draw_text_clipped(f.bold, picker.x + 56, picker.y + 25, VERSIONS[self.version].0, picker.w - 90, TEXT);
        c.draw_text_clipped(f.ui, picker.x + 56, picker.y + 44, VERSIONS[self.version].1, picker.w - 90, DIM);
        let (cx, cy) = (picker.right() - 22, picker.y + 26);
        for k in 0..5 {
            c.fill_rect(Rect::new(cx - 5 + k, cy + 4 - k, 11 - 2 * k, 1), DIM);
        }
        let pw = 260.min((bar.w - 600).max(200));
        let play = Rect::new((bar.x + (bar.w - pw) / 2).max(picker.right() + 24), bar.y + 16, pw, 60);
        let hov = self.push(play, Hit::Play);
        c.fill_rect(play, if hov { GREEN_HOVER } else { GREEN });
        c.fill_rect(Rect::new(play.x, play.bottom() - 4, play.w, 4), rgb(0x27, 0x59, 0x1a));
        c.draw_text_centered(f.pixel, play, if installed() { "PLAY" } else { "INSTALL" }, TEXT);
        let who = match &self.account {
            Some(n) => format!("{}", n),
            None => format!("{} (offline)", self.name.text),
        };
        let ww = f.ui.measure(&who).min(220);
        c.draw_text(f.ui, bar.right() - 24 - ww, bar.y + 42, "Playing as", DIM);
        c.draw_text_clipped(f.heavy, bar.right() - 24 - ww, bar.y + 62, &who, 220, TEXT);

        if self.picker_open {
            let ih = 56;
            let list = Rect::new(picker.x, picker.y - 8 - ih * VERSIONS.len() as i32 - 8, 320, ih * VERSIONS.len() as i32 + 8);
            self.hits.push((Rect::new(0, 0, self.size.0, self.size.1), Hit::Backdrop));
            c.draw_shadow(list, 0, 16, with_alpha(0x000000, 120));
            c.fill_rect(list, rgb(0x2a, 0x2a, 0x2a));
            for i in 0..VERSIONS.len() {
                let r = Rect::new(list.x + 4, list.y + 4 + i as i32 * ih, list.w - 8, ih);
                let hov = self.push(r, Hit::PickVersion(i));
                if hov || i == self.version {
                    c.fill_rect(r, if hov { CARD_HOVER } else { rgb(0x35, 0x35, 0x35) });
                }
                c.draw_text(f.bold, r.x + 14, r.y + 24, VERSIONS[i].0, TEXT);
                c.draw_text(f.ui, r.x + 14, r.y + 43, VERSIONS[i].1, DIM);
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
            c.draw_text(f.bold, r.x + 56, r.y + 28, VERSIONS[i].0, TEXT);
            c.draw_text(f.ui, r.x + 56, r.y + 48, VERSIONS[i].1, DIM);
            let b = Rect::new(r.right() - 110, r.y + 14, 100, 36);
            let hov = self.push(b, Hit::InstallPlay(i));
            c.fill_rect(b, if hov { GREEN_HOVER } else { GREEN });
            c.draw_text_centered(f.bold, b, if installed() { "Play" } else { "Install" }, TEXT);
            y += 72;
        }
        let b = Rect::new(x + 28, y + 16, 220, 36);
        let hov = self.push(b, Hit::Folder);
        c.fill_rect(b, if hov { CARD_HOVER } else { CARD });
        c.draw_text_centered(f.bold, b, "Open game folder", TEXT);
    }

    fn render_settings(&mut self, c: &mut Canvas, x: i32, w: i32, h: i32) {
        let f = fonts();
        c.fill_rect(Rect::new(x, HEADER_H, w - x, h - HEADER_H), BG);
        let lx = x + 28;
        let mut y = HEADER_H + 30;
        let label = |c: &mut Canvas, y: i32, t: &str, d: &str| {
            c.draw_text(f.bold, lx, y, t, TEXT);
            c.draw_text(f.ui, lx, y + 20, d, DIM);
        };
        label(c, y, "Player name", "Used when you play offline");
        let r = Rect::new(w - 28 - 300, y - 18, 300, 36);
        self.push(r, Hit::Name);
        text_field(c, r, &self.name.text.clone(), self.editing == Some(Hit::Name));
        y += 64;
        label(c, y, "Memory", "How much RAM the game may use");
        for i in 0..MEMORY.len() {
            let r = Rect::new(w - 28 - 300 + i as i32 * 76, y - 18, 72, 36);
            let hov = self.push(r, Hit::Memory(i));
            let sel = self.memory == i;
            c.fill_rect(r, if sel { GREEN } else if hov { CARD_HOVER } else { CARD });
            c.draw_text_centered(f.bold, r, MEMORY[i], TEXT);
        }
        y += 64;
        label(c, y, "Fullscreen", "Start 1.8.9 and older versions fullscreen");
        let t = Rect::new(w - 28 - 24, y - 12, 24, 24);
        let hov = self.push(t.inset(-6), Hit::Fullscreen);
        c.fill_rect(t, if self.fullscreen { GREEN } else if hov { rgb(0x55, 0x55, 0x55) } else { rgb(0x1b, 0x1b, 0x1b) });
        if !self.fullscreen {
            for (rx, ry, rw, rh) in [(0, 0, 24, 1), (0, 23, 24, 1), (0, 0, 1, 24), (23, 0, 1, 24)] {
                c.fill_rect(Rect::new(t.x + rx, t.y + ry, rw, rh), LINE);
            }
        } else {
            for d in 0..5 {
                c.fill_rect(Rect::new(t.x + 5 + d, t.y + 11 + d, 2, 2), TEXT);
            }
            for d in 0..9 {
                c.fill_rect(Rect::new(t.x + 9 + d, t.y + 15 - d, 2, 2), TEXT);
            }
        }
        y += 64;
        label(c, y, "Microsoft app ID", "The app used to sign in (leave as is unless you have your own)");
        let r = Rect::new(w - 28 - 300, y - 18, 300, 36);
        self.push(r, Hit::ClientId);
        text_field(c, r, &self.client_id.text.clone(), self.editing == Some(Hit::ClientId));
        y += 60;
        let _ = y;
    }

    fn render_login(&mut self, c: &mut Canvas, w: i32, h: i32) {
        let f = fonts();
        let Some(l) = &self.login else { return };
        let (code, url, status, failed) = (l.code.clone(), l.url.clone(), l.status.clone(), l.failed);
        self.hits.push((Rect::new(0, 0, w, h), Hit::Backdrop));
        c.fill_rect(Rect::new(0, 0, w, h), rgba(0, 0, 0, 150));
        let r = Rect::new((w - 460) / 2, (h - 300) / 2, 460, 300);
        c.draw_shadow(r, 0, 20, with_alpha(0x000000, 140));
        c.fill_rect(r, rgb(0x2a, 0x2a, 0x2a));
        c.draw_text_centered(f.large, Rect::new(r.x, r.y + 20, r.w, 36), "Sign in with Microsoft", TEXT);
        if code.is_empty() {
            c.draw_text_centered(f.ui, Rect::new(r.x + 20, r.y + 110, r.w - 40, 24), &status, if failed { rgb(0xff, 0x8a, 0x80) } else { DIM });
        } else {
            c.draw_text_centered(f.ui, Rect::new(r.x, r.y + 70, r.w, 20), "Firefox opened the sign-in page. If it asks, enter:", DIM);
            let cr = Rect::new(r.x + 60, r.y + 100, r.w - 120, 60);
            c.fill_rect(cr, rgb(0x1b, 0x1b, 0x1b));
            c.draw_text_centered(f.large, cr, &code, TEXT);
            c.draw_text_centered(f.ui, Rect::new(r.x, r.y + 172, r.w, 20), &format!("at {}", url), DIM);
            c.draw_text_centered(f.ui, Rect::new(r.x + 20, r.y + 196, r.w - 40, 20), &status, if failed { rgb(0xff, 0x8a, 0x80) } else { DIM });
            let b = Rect::new(r.x + 30, r.bottom() - 60, 190, 40);
            let hov = self.push(b, Hit::OpenPage);
            c.fill_rect(b, if hov { GREEN_HOVER } else { GREEN });
            c.draw_text_centered(f.bold, b, "Open page again", TEXT);
        }
        let b = if code.is_empty() { Rect::new(r.x + (r.w - 190) / 2, r.bottom() - 60, 190, 40) } else { Rect::new(r.right() - 220, r.bottom() - 60, 190, 40) };
        let hov = self.push(b, Hit::CancelLogin);
        c.fill_rect(b, if hov { CARD_HOVER } else { rgb(0x3a, 0x3a, 0x3a) });
        c.draw_text_centered(f.bold, b, if failed { "Close" } else { "Cancel" }, TEXT);
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
        let now = crate::time::uptime_ms();
        if self.panorama.is_none() && now - self.art_checked > 3000 {
            self.art_checked = now;
            if fs::exists(&format!("{}/panorama.png", ART_DIR)) {
                self.reload_art();
                ctx.redraw();
            }
        }
        if self.poll_login() {
            ctx.redraw();
        }
    }
}
