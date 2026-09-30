//! The Minecraft launcher: pick a version and mod pack, a player name or a
//! Microsoft account, memory and window mode, then Play. The game itself
//! is started by the `minecraft` command (MayOS's Java launcher) in a
//! terminal window that shows its progress.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use gfx::icons::Icon;
use gfx::{rgb, rgba, Canvas, Rect};

use super::app::{App, AppEvent, AppKind, Ctx};
use super::theme::{self, fonts};
use super::widgets::{self, ButtonStyle, TextInput};
use crate::fs;

const CONFIG: &str = "/config/minecraft.cfg";

/// (label, detail, arguments for `minecraft`).
const VERSIONS: &[(&str, &str, &str)] = &[
    ("1.21.11", "Fabric + Sodium and friends (fastest)", "1.21.11 --mods"),
    ("1.21.11", "Vanilla", "1.21.11 --vanilla"),
    ("1.8.9", "Forge + OptiFine (PvP)", "1.8.9"),
    ("Latest", "Newest release this Java can run", ""),
];
const MEMORY: &[&str] = &["2G", "4G", "6G", "8G"];

#[derive(Clone, Copy, PartialEq, Eq)]
enum Hit {
    Version(usize),
    Memory(usize),
    Name,
    Fullscreen,
    SignIn,
    SignOut,
    Play,
}

pub struct McLauncher {
    version: usize,
    memory: usize,
    fullscreen: bool,
    name: TextInput,
    editing: bool,
    hover: Option<Hit>,
    size: (i32, i32),
}

fn installed() -> bool {
    fs::exists("/usr/bin/minecraft")
}

fn account() -> Option<String> {
    let text = fs::read_file("/home/.minecraft/mayos-account.json").ok()?;
    let text = String::from_utf8_lossy(&text).into_owned();
    let at = text.find("\"name\":\"")? + 8;
    Some(String::from(&text[at..at + text[at..].find('"')?]))
}

impl McLauncher {
    pub fn new() -> McLauncher {
        let mut l = McLauncher { version: 0, memory: 1, fullscreen: false, name: TextInput::new("Player"), editing: false, hover: None, size: (860, 540) };
        if let Ok(d) = fs::read_file(CONFIG) {
            for line in String::from_utf8_lossy(&d).lines() {
                match line.split_once('=') {
                    Some(("version", v)) => l.version = v.parse().unwrap_or(0).min(VERSIONS.len() - 1),
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
        let text = format!("version={}\nmemory={}\nfullscreen={}\nname={}\n", self.version, self.memory, self.fullscreen as u8, self.name.text);
        let _ = fs::write_file(CONFIG, text.as_bytes());
        let _ = fs::create_dir("/etc");
        let _ = fs::write_file("/etc/minecraft-user", self.name.text.as_bytes());
    }

    fn layout(&self) -> Vec<(Rect, Hit)> {
        let (w, h) = self.size;
        let x0 = 300;
        let mut out = Vec::new();
        let cw = (w - x0 - 40) / 2 - 6;
        for i in 0..VERSIONS.len() {
            let (cx, cy) = (x0 + 20 + (i as i32 % 2) * (cw + 12), 64 + (i as i32 / 2) * 70);
            out.push((Rect::new(cx, cy, cw, 60), Hit::Version(i)));
        }
        out.push((Rect::new(x0 + 20, 244, 220, 34), Hit::Name));
        if account().is_some() {
            out.push((Rect::new(x0 + 252, 244, 120, 34), Hit::SignOut));
        } else {
            out.push((Rect::new(x0 + 252, 244, 200, 34), Hit::SignIn));
        }
        for i in 0..MEMORY.len() {
            out.push((Rect::new(x0 + 20 + i as i32 * 74, 330, 66, 32), Hit::Memory(i)));
        }
        out.push((Rect::new(x0 + 20, 400, 240, 32), Hit::Fullscreen));
        out.push((Rect::new(w - 200, h - 70, 176, 48), Hit::Play));
        out
    }

    fn play(&mut self, ctx: &mut Ctx) {
        self.save();
        let cmd = if !installed() {
            String::from("pkg install minecraft")
        } else {
            let mut c = format!("minecraft {} --memory {}", VERSIONS[self.version].2, MEMORY[self.memory]);
            if self.fullscreen {
                c.push_str(" --fullscreen");
            }
            c
        };
        ctx.open(alloc::boxed::Box::new(super::terminal::Terminal::with_command("/home", &cmd)));
    }
}

impl App for McLauncher {
    fn title(&self) -> String {
        String::from("Minecraft")
    }
    fn icon(&self) -> Icon {
        Icon::Minecraft
    }
    fn kind(&self) -> AppKind {
        AppKind::Minecraft
    }
    fn initial_size(&self) -> (i32, i32) {
        (860, 540)
    }
    fn min_size(&self) -> (i32, i32) {
        (820, 500)
    }

    fn render(&mut self, c: &mut Canvas, (w, h): (i32, i32), focused: bool) {
        self.size = (w, h);
        let f = fonts();
        let white = rgb(255, 255, 255);
        // Hero panel: grass over dirt.
        let hero = Rect::new(0, 0, 300, h);
        c.fill_gradient_v(Rect::new(0, 0, 300, h * 2 / 3), rgb(0x5e, 0x9e, 0x3a), rgb(0x3b, 0x6e, 0x24));
        c.fill_gradient_v(Rect::new(0, h * 2 / 3, 300, h - h * 2 / 3), rgb(0x79, 0x55, 0x3a), rgb(0x4a, 0x33, 0x22));
        for k in 0..30 {
            let (x, y) = ((k * 97) % 300, h * 2 / 3 + (k * 53) % (h / 3));
            c.fill_rect(Rect::new(x, y, 8, 8), rgba(0, 0, 0, 40));
        }
        super::icons::draw(c, Icon::Minecraft, 86, 70, 128);
        c.draw_text_centered(&f.large, Rect::new(0, 214, hero.w, 40), "Minecraft", white);
        c.draw_text_centered(&f.ui, Rect::new(0, 250, hero.w, 24), "Java Edition \u{00b7} MayOS Launcher", rgba(255, 255, 255, 210));
        let acct = account();
        let who = match &acct {
            Some(n) => format!("Signed in as {}", n),
            None => format!("Offline as {}", self.name.text),
        };
        c.draw_text_centered(&f.bold, Rect::new(0, h - 60, hero.w, 24), &who, white);

        // Settings side.
        let x0 = 300;
        c.fill_rect(Rect::new(x0, 0, w - x0, h), theme::WINDOW_BG);
        let label = |c: &mut Canvas, y: i32, t: &str| c.draw_text(&f.small_bold, x0 + 20, y, t, theme::TEXT_DIM);
        label(c, 50, "VERSION");
        label(c, 232, "PLAYER");
        label(c, 318, "MEMORY");
        label(c, 390, "WINDOW");
        for (r, hit) in self.layout() {
            let hov = self.hover == Some(hit);
            match hit {
                Hit::Version(i) => {
                    let sel = self.version == i;
                    c.fill_rounded_rect(r, 10, if sel { theme::selection() } else if hov { theme::HOVER } else { theme::PANEL_BG });
                    if sel {
                        c.stroke_rounded_rect(r, 10, 2, theme::accent());
                    }
                    c.draw_text(&f.bold, r.x + 14, r.y + 25, VERSIONS[i].0, theme::TEXT);
                    c.draw_text_clipped(&f.ui, r.x + 14, r.y + 46, VERSIONS[i].1, r.w - 24, theme::TEXT_DIM);
                }
                Hit::Name => {
                    c.fill_rounded_rect(r, 8, if acct.is_some() { theme::PANEL_BG } else { rgb(255, 255, 255) });
                    c.stroke_rounded_rect(r, 8, if self.editing { 2 } else { 1 }, if self.editing { theme::accent() } else { theme::SEPARATOR });
                    self.name.render(c, r.inset(6), self.editing && focused);
                }
                Hit::SignIn => widgets::button(c, r, "Sign in with Microsoft", ButtonStyle::Normal, hov, true),
                Hit::SignOut => widgets::button(c, r, "Sign out", ButtonStyle::Normal, hov, true),
                Hit::Memory(i) => {
                    let style = if self.memory == i { ButtonStyle::Primary } else { ButtonStyle::Normal };
                    widgets::button(c, r, MEMORY[i], style, hov, true);
                }
                Hit::Fullscreen => {
                    let b = Rect::new(r.x, r.y + 7, 18, 18);
                    if self.fullscreen {
                        c.fill_rounded_rect(b, 5, theme::accent());
                        for d in 0..4 {
                            c.fill_rect(Rect::new(b.x + 4 + d, b.y + 8 + d, 2, 2), white);
                        }
                        for d in 0..7 {
                            c.fill_rect(Rect::new(b.x + 7 + d, b.y + 11 - d, 2, 2), white);
                        }
                    } else {
                        c.stroke_rounded_rect(b, 5, 2, if hov { theme::accent() } else { theme::SEPARATOR });
                    }
                    c.draw_text(&f.ui, r.x + 28, r.y + 21, "Fullscreen (1.8.9 and older)", theme::TEXT);
                }
                Hit::Play => {
                    let bg = if hov { rgb(0x2e, 0x8b, 0x3a) } else { rgb(0x3c, 0xa8, 0x48) };
                    c.fill_rounded_rect(r, 12, bg);
                    let t = if installed() { "PLAY" } else { "INSTALL" };
                    c.draw_text_centered(&f.large, r, t, white);
                }
            }
        }
        let note = if installed() {
            "Mods, worlds and screenshots: /home/.minecraft (Files \u{2192} Home)."
        } else {
            "Minecraft is not installed yet: Install downloads Java and the game (about 700 MB)."
        };
        c.draw_text_clipped(&f.ui, x0 + 20, h - 40, note, w - x0 - 240, theme::TEXT_DIM);
    }

    fn event(&mut self, ev: &AppEvent, ctx: &mut Ctx) {
        match *ev {
            AppEvent::MouseMove { x, y, .. } => {
                let h = self.layout().into_iter().find(|(r, _)| r.contains(x, y)).map(|(_, h)| h);
                if h != self.hover {
                    self.hover = h;
                    ctx.redraw();
                }
            }
            AppEvent::MouseDown { x, y, .. } => {
                let hit = self.layout().into_iter().find(|(r, _)| r.contains(x, y)).map(|(_, h)| h);
                self.editing = hit == Some(Hit::Name) && account().is_none();
                match hit {
                    Some(Hit::Version(i)) => self.version = i,
                    Some(Hit::Memory(i)) => self.memory = i,
                    Some(Hit::Fullscreen) => self.fullscreen = !self.fullscreen,
                    Some(Hit::SignIn) => {
                        ctx.open(alloc::boxed::Box::new(super::terminal::Terminal::with_command("/home", "minecraft --login")));
                    }
                    Some(Hit::SignOut) => {
                        let _ = fs::remove("/home/.minecraft/mayos-account.json");
                    }
                    Some(Hit::Play) => self.play(ctx),
                    _ => {}
                }
                self.save();
                ctx.redraw();
            }
            AppEvent::Key(ref k) if self.editing => {
                if k.pressed && k.key == crate::input::Key::Enter {
                    self.editing = false;
                } else {
                    self.name.key(k);
                    self.name.text.retain(|ch| ch.is_ascii_alphanumeric() || ch == '_');
                    self.name.text.truncate(16);
                    self.name.caret = self.name.caret.min(self.name.text.len());
                }
                self.save();
                ctx.redraw();
            }
            _ => {}
        }
    }
}
