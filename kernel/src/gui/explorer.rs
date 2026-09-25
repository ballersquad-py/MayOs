//! File explorer: browse, open, create, rename, copy, move and delete files
//! on the FAT32 disk.

use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use gfx::icons::{self, Icon};
use gfx::{rgb, with_alpha, Canvas, Rect};

use super::app::{App, AppEvent, AppKind, Ctx, Msg};
use super::dialog::Dialog;
use super::terminal::Terminal;
use super::theme::{self, fonts};
use super::widgets::{self, button, ButtonStyle};
use crate::fs::{self, DirEntry};
use crate::input::Key;
use crate::sync::Spin;

const TOOLBAR_H: i32 = 50;
const SIDEBAR_W: i32 = 176;
const HEADER_H: i32 = 28;
const ROW_H: i32 = 28;
const STATUS_H: i32 = 26;

const TAG_NEW_FOLDER: u32 = 1;
const TAG_NEW_FILE: u32 = 2;
const TAG_RENAME: u32 = 3;
const TAG_DELETE: u32 = 4;

/// Shared between explorer windows: (path, is_cut).
static CLIPBOARD: Spin<Option<(String, bool)>> = Spin::new(None);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Tool {
    Back,
    Forward,
    Up,
    NewFolder,
    NewFile,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Action {
    Open,
    Rename,
    Duplicate,
    Copy,
    Cut,
    Paste,
    Delete,
    NewFolder,
    NewFile,
    Refresh,
    TerminalHere,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Hover {
    None,
    Tool(Tool),
    Place(usize),
    Crumb(usize),
    Row(usize),
    Menu(usize),
}

struct ContextMenu {
    x: i32,
    y: i32,
    items: Vec<Option<(Action, &'static str)>>,
}

impl ContextMenu {
    fn rect(&self, bounds: (i32, i32)) -> Rect {
        let h: i32 = self.items.iter().map(|i| if i.is_some() { 26 } else { 9 }).sum::<i32>() + 10;
        let w = 190;
        let x = self.x.min(bounds.0 - w - 4).max(4);
        let y = self.y.min(bounds.1 - h - 4).max(4);
        Rect::new(x, y, w, h)
    }

    fn item_at(&self, bounds: (i32, i32), x: i32, y: i32) -> Option<usize> {
        let r = self.rect(bounds);
        if !r.contains(x, y) {
            return None;
        }
        let mut yy = r.y + 5;
        for (i, it) in self.items.iter().enumerate() {
            let h = if it.is_some() { 26 } else { 9 };
            if y >= yy && y < yy + h {
                return it.map(|_| i);
            }
            yy += h;
        }
        None
    }
}

pub struct Explorer {
    path: String,
    entries: Vec<DirEntry>,
    selected: Option<usize>,
    scroll: i32,
    back: Vec<String>,
    forward: Vec<String>,
    generation: u64,
    size: (i32, i32),
    hover: Hover,
    menu: Option<ContextMenu>,
    message: Option<(String, bool, u64)>,
    pending: Option<String>,
    select_after_refresh: Option<String>,
    dragging_scrollbar: bool,
    focused: bool,
}

fn kind_of(e: &DirEntry, dir: &str) -> (Icon, String) {
    if e.is_dir {
        return (Icon::Folder, String::from("Folder"));
    }
    let ext = fs::extension(&e.name);
    match ext.as_deref() {
        Some("txt" | "md" | "log" | "cfg" | "ini" | "conf" | "rs" | "c" | "h" | "json" | "toml" | "sh" | "html"
        | "css" | "js") => (Icon::TextFile, String::from("Text Document")),
        Some("png" | "jpg" | "jpeg" | "bmp" | "gif" | "webp") => (Icon::Image, String::from("Image")),
        Some("elf") => (Icon::Program, String::from("Program")),
        _ if dir == "/bin" => (Icon::Program, String::from("Program")),
        Some(x) => (Icon::File, format!("{} File", x.to_ascii_uppercase())),
        None => (Icon::File, String::from("File")),
    }
}

impl Explorer {
    pub fn new(path: &str) -> Explorer {
        let path = if fs::is_dir(path) { String::from(path) } else { String::from("/") };
        let mut e = Explorer {
            path,
            entries: Vec::new(),
            selected: None,
            scroll: 0,
            back: Vec::new(),
            forward: Vec::new(),
            generation: 0,
            size: (760, 480),
            hover: Hover::None,
            menu: None,
            message: None,
            pending: None,
            select_after_refresh: None,
            dragging_scrollbar: false,
            focused: true,
        };
        e.refresh();
        e
    }

    fn refresh(&mut self) {
        self.generation = fs::generation();
        let keep = self
            .select_after_refresh
            .take()
            .or_else(|| self.selected.and_then(|i| self.entries.get(i)).map(|e| e.name.clone()));
        match fs::read_dir(&self.path) {
            Ok(entries) => self.entries = entries,
            Err(e) => {
                self.entries.clear();
                self.flash(format!("Cannot open {}: {}", self.path, e), true);
            }
        }
        self.selected = keep.and_then(|n| self.entries.iter().position(|e| e.name == n));
        if let Some(i) = self.selected {
            self.ensure_visible(i);
        }
    }

    fn navigate(&mut self, path: &str, record: bool) {
        let path = fs::normalize("/", path);
        if !fs::is_dir(&path) {
            self.flash(format!("{} is not a folder", path), true);
            return;
        }
        if record && path != self.path {
            self.back.push(core::mem::replace(&mut self.path, path));
            self.forward.clear();
        } else {
            self.path = path;
        }
        self.selected = None;
        self.scroll = 0;
        self.menu = None;
        self.refresh();
    }

    fn go_back(&mut self) {
        if let Some(p) = self.back.pop() {
            self.forward.push(core::mem::replace(&mut self.path, p));
            let p = self.path.clone();
            self.navigate(&p, false);
        }
    }

    fn go_forward(&mut self) {
        if let Some(p) = self.forward.pop() {
            self.back.push(core::mem::replace(&mut self.path, p));
            let p = self.path.clone();
            self.navigate(&p, false);
        }
    }

    fn go_up(&mut self) {
        if self.path != "/" {
            let child = fs::file_name(&self.path).to_string();
            let parent = fs::parent(&self.path);
            self.navigate(&parent, true);
            self.selected = self.entries.iter().position(|e| e.name == child);
        }
    }

    fn flash(&mut self, msg: String, error: bool) {
        self.message = Some((msg, error, crate::time::uptime_ms() + 4000));
    }

    fn selected_path(&self) -> Option<String> {
        self.selected.and_then(|i| self.entries.get(i)).map(|e| fs::join(&self.path, &e.name))
    }

    // ---------------------------------------------------------------
    // Layout
    // ---------------------------------------------------------------

    fn tool_rect(&self, t: Tool) -> Rect {
        let w = self.size.0;
        match t {
            Tool::Back => Rect::new(12, 10, 32, 30),
            Tool::Forward => Rect::new(46, 10, 32, 30),
            Tool::Up => Rect::new(80, 10, 32, 30),
            Tool::NewFile => Rect::new(w - 96, 10, 84, 30),
            Tool::NewFolder => Rect::new(w - 196, 10, 94, 30),
        }
    }

    fn crumb_area(&self) -> Rect {
        Rect::new(124, 10, (self.size.0 - 124 - 208).max(60), 30)
    }

    fn crumbs(&self) -> Vec<(String, String, Rect)> {
        let f = &fonts().ui;
        let area = self.crumb_area();
        let mut out = Vec::new();
        let mut x = area.x + 10;
        let mut acc = String::from("/");
        let mut parts = alloc::vec![(String::from("MayOS Disk"), String::from("/"))];
        for p in self.path.split('/').filter(|s| !s.is_empty()) {
            acc = fs::join(&acc, p);
            parts.push((String::from(p), acc.clone()));
        }
        // If too long, keep the last segments.
        let sep_w = f.measure(" \u{203a} ");
        let total: i32 = parts.iter().map(|(n, _)| f.measure(n) + 12 + sep_w).sum();
        let mut skip = 0;
        let mut remaining = total;
        while remaining > area.w - 20 && skip + 1 < parts.len() {
            remaining -= f.measure(&parts[skip].0) + 12 + sep_w;
            skip += 1;
        }
        for (name, path) in parts.into_iter().skip(skip) {
            let w = f.measure(&name) + 12;
            out.push((name, path, Rect::new(x, area.y + 3, w, area.h - 6)));
            x += w + sep_w;
        }
        out
    }

    fn places(&self) -> Vec<(&'static str, &'static str, Icon)> {
        let mut v = alloc::vec![("MayOS Disk", "/", Icon::Drive)];
        for (label, path, icon) in [
            ("Documents", "/docs", Icon::Folder),
            ("Programs", "/bin", Icon::Folder),
            ("Pictures", "/pictures", Icon::Folder),
            ("Home", "/home", Icon::Home),
        ] {
            if fs::is_dir(path) {
                v.push((label, path, icon));
            }
        }
        v
    }

    fn place_rect(&self, i: usize) -> Rect {
        Rect::new(8, TOOLBAR_H + 34 + i as i32 * 30, SIDEBAR_W - 16, 28)
    }

    fn list_rect(&self) -> Rect {
        let (w, h) = self.size;
        Rect::new(SIDEBAR_W, TOOLBAR_H + HEADER_H, w - SIDEBAR_W, h - TOOLBAR_H - HEADER_H - STATUS_H)
    }

    fn row_rect(&self, i: usize) -> Rect {
        let l = self.list_rect();
        Rect::new(l.x + 6, l.y + 4 + i as i32 * ROW_H - self.scroll, l.w - 20, ROW_H)
    }

    fn content_height(&self) -> i32 {
        self.entries.len() as i32 * ROW_H + 8
    }

    fn scrollbar_track(&self) -> Rect {
        let l = self.list_rect();
        Rect::new(l.right() - 12, l.y + 2, 10, l.h - 4)
    }

    fn clamp_scroll(&mut self) {
        let max = (self.content_height() - self.list_rect().h).max(0);
        self.scroll = self.scroll.clamp(0, max);
    }

    fn ensure_visible(&mut self, i: usize) {
        let l = self.list_rect();
        let top = 4 + i as i32 * ROW_H;
        if top < self.scroll {
            self.scroll = top - 4;
        } else if top + ROW_H > self.scroll + l.h {
            self.scroll = top + ROW_H - l.h + 4;
        }
        self.clamp_scroll();
    }

    fn row_at(&self, x: i32, y: i32) -> Option<usize> {
        let l = self.list_rect();
        if !l.contains(x, y) || x >= l.right() - 14 {
            return None;
        }
        let i = (y - l.y - 4 + self.scroll) / ROW_H;
        if y - l.y - 4 + self.scroll < 0 || i < 0 || i as usize >= self.entries.len() {
            return None;
        }
        Some(i as usize)
    }

    fn hit(&self, x: i32, y: i32) -> Hover {
        if let Some(m) = &self.menu {
            return match m.item_at(self.size, x, y) {
                Some(i) => Hover::Menu(i),
                None => Hover::None,
            };
        }
        for t in [Tool::Back, Tool::Forward, Tool::Up, Tool::NewFolder, Tool::NewFile] {
            if self.tool_rect(t).contains(x, y) {
                return Hover::Tool(t);
            }
        }
        for (i, (_, _, r)) in self.crumbs().iter().enumerate() {
            if r.contains(x, y) {
                return Hover::Crumb(i);
            }
        }
        for i in 0..self.places().len() {
            if self.place_rect(i).contains(x, y) {
                return Hover::Place(i);
            }
        }
        match self.row_at(x, y) {
            Some(i) => Hover::Row(i),
            None => Hover::None,
        }
    }

    // ---------------------------------------------------------------
    // Actions
    // ---------------------------------------------------------------

    fn open_entry(&mut self, i: usize, ctx: &mut Ctx) {
        let Some(e) = self.entries.get(i) else { return };
        let path = fs::join(&self.path, &e.name);
        if e.is_dir {
            self.navigate(&path, true);
        } else if let Err(err) = super::open_path(&path, ctx) {
            self.flash(format!("Cannot open {}: {}", e.name, err), true);
        }
    }

    fn run(&mut self, a: Action, ctx: &mut Ctx) {
        let me = ctx.window;
        match a {
            Action::Open => {
                if let Some(i) = self.selected {
                    self.open_entry(i, ctx);
                }
            }
            Action::NewFolder => {
                let name = fs::unique_name(&self.path, "New Folder");
                ctx.open_child(Box::new(Dialog::input("New Folder", "Name for the new folder:", &name, "Create", TAG_NEW_FOLDER, me)));
            }
            Action::NewFile => {
                let name = fs::unique_name(&self.path, "New Text File.txt");
                ctx.open_child(Box::new(Dialog::input("New File", "Name for the new file:", &name, "Create", TAG_NEW_FILE, me)));
            }
            Action::Rename => {
                if let Some(p) = self.selected_path() {
                    let name = fs::file_name(&p).to_string();
                    self.pending = Some(p);
                    ctx.open_child(Box::new(Dialog::input("Rename", &format!("New name for \u{201c}{}\u{201d}:", name), &name, "Rename", TAG_RENAME, me)));
                }
            }
            Action::Delete => {
                if let Some(p) = self.selected_path() {
                    let name = fs::file_name(&p).to_string();
                    let what = if fs::is_dir(&p) { "folder and everything in it" } else { "file" };
                    self.pending = Some(p);
                    ctx.open_child(Box::new(Dialog::confirm(
                        "Delete",
                        &format!("Delete the {} \u{201c}{}\u{201d}?", what, name),
                        "Delete",
                        true,
                        TAG_DELETE,
                        me,
                    )));
                }
            }
            Action::Duplicate => {
                if let Some(p) = self.selected_path() {
                    let name = fs::file_name(&p);
                    let (stem, ext) = match name.rfind('.') {
                        Some(i) if i > 0 && !fs::is_dir(&p) => (&name[..i], &name[i..]),
                        _ => (name, ""),
                    };
                    let target = fs::unique_name(&self.path, &format!("{} copy{}", stem, ext));
                    match fs::copy(&p, &fs::join(&self.path, &target)) {
                        Ok(()) => self.select_after_refresh = Some(target),
                        Err(e) => self.flash(format!("Duplicate failed: {}", e), true),
                    }
                }
            }
            Action::Copy | Action::Cut => {
                if let Some(p) = self.selected_path() {
                    let cut = a == Action::Cut;
                    self.flash(format!("{} \u{201c}{}\u{201d}", if cut { "Cut" } else { "Copied" }, fs::file_name(&p)), false);
                    *CLIPBOARD.lock() = Some((p, cut));
                }
            }
            Action::Paste => self.paste(),
            Action::Refresh => self.refresh(),
            Action::TerminalHere => {
                let mut t = Terminal::new();
                t.cwd = self.path.clone();
                ctx.open(Box::new(t));
            }
        }
        ctx.redraw();
    }

    fn paste(&mut self) {
        let Some((src, cut)) = CLIPBOARD.lock().clone() else { return };
        if !fs::exists(&src) {
            self.flash(String::from("The copied item no longer exists"), true);
            *CLIPBOARD.lock() = None;
            return;
        }
        let name = fs::file_name(&src).to_string();
        if cut {
            if fs::parent(&src) == self.path {
                return;
            }
            let target = fs::unique_name(&self.path, &name);
            match fs::rename(&src, &fs::join(&self.path, &target)) {
                Ok(()) => {
                    *CLIPBOARD.lock() = None;
                    self.select_after_refresh = Some(target);
                }
                Err(e) => self.flash(format!("Move failed: {}", e), true),
            }
        } else {
            let target = fs::unique_name(&self.path, &name);
            match fs::copy(&src, &fs::join(&self.path, &target)) {
                Ok(()) => self.select_after_refresh = Some(target),
                Err(e) => self.flash(format!("Paste failed: {}", e), true),
            }
        }
    }

    fn dialog_result(&mut self, tag: u32, value: Option<String>) {
        let pending = self.pending.take();
        let Some(value) = value else { return };
        let name = value.trim();
        match tag {
            TAG_NEW_FOLDER | TAG_NEW_FILE | TAG_RENAME if name.is_empty() => {
                self.flash(String::from("A name is required"), true);
            }
            TAG_NEW_FOLDER => match fs::create_dir(&fs::join(&self.path, name)) {
                Ok(()) => self.select_after_refresh = Some(name.to_string()),
                Err(e) => self.flash(format!("Cannot create folder: {}", e), true),
            },
            TAG_NEW_FILE => match fs::create_file(&fs::join(&self.path, name)) {
                Ok(()) => self.select_after_refresh = Some(name.to_string()),
                Err(e) => self.flash(format!("Cannot create file: {}", e), true),
            },
            TAG_RENAME => {
                if let Some(from) = pending {
                    let to = fs::join(&fs::parent(&from), name);
                    if to != from {
                        match fs::rename(&from, &to) {
                            Ok(()) => self.select_after_refresh = Some(name.to_string()),
                            Err(e) => self.flash(format!("Cannot rename: {}", e), true),
                        }
                    }
                }
            }
            TAG_DELETE => {
                if let Some(p) = pending {
                    let r = if fs::is_dir(&p) { fs::remove_all(&p) } else { fs::remove(&p) };
                    match r {
                        Ok(()) => self.flash(format!("Deleted \u{201c}{}\u{201d}", fs::file_name(&p)), false),
                        Err(e) => self.flash(format!("Cannot delete: {}", e), true),
                    }
                }
            }
            _ => {}
        }
        self.refresh();
    }

    fn open_menu(&mut self, x: i32, y: i32, on_item: bool) {
        let has_clip = CLIPBOARD.lock().is_some();
        let mut items: Vec<Option<(Action, &'static str)>> = Vec::new();
        if on_item {
            items.push(Some((Action::Open, "Open")));
            items.push(None);
            items.push(Some((Action::Rename, "Rename\u{2026}")));
            items.push(Some((Action::Duplicate, "Duplicate")));
            items.push(Some((Action::Copy, "Copy")));
            items.push(Some((Action::Cut, "Cut")));
            items.push(Some((Action::Delete, "Delete\u{2026}")));
            items.push(None);
        }
        items.push(Some((Action::NewFolder, "New Folder\u{2026}")));
        items.push(Some((Action::NewFile, "New File\u{2026}")));
        if has_clip {
            items.push(Some((Action::Paste, "Paste")));
        }
        items.push(None);
        items.push(Some((Action::TerminalHere, "Open Terminal Here")));
        items.push(Some((Action::Refresh, "Refresh")));
        self.menu = Some(ContextMenu { x, y, items });
        self.hover = Hover::None;
    }

    // ---------------------------------------------------------------
    // Drawing
    // ---------------------------------------------------------------

    fn draw_toolbar(&self, c: &mut Canvas) {
        let f = fonts();
        let w = self.size.0;
        c.fill_rect(Rect::new(0, 0, w, TOOLBAR_H), theme::PANEL_BG);
        c.hline(0, TOOLBAR_H - 1, w, theme::SEPARATOR);
        let tools = [
            (Tool::Back, "\u{25c0}", !self.back.is_empty()),
            (Tool::Forward, "\u{25b6}", !self.forward.is_empty()),
            (Tool::Up, "\u{25b2}", self.path != "/"),
        ];
        for (t, glyph, enabled) in tools {
            let r = self.tool_rect(t);
            let hovered = self.hover == Hover::Tool(t) && enabled;
            button(c, r, glyph, ButtonStyle::Flat, hovered, enabled);
        }
        // Breadcrumb bar.
        let area = self.crumb_area();
        c.fill_rounded_rect(area, 8, rgb(0xff, 0xff, 0xff));
        c.stroke_rounded_rect(area, 8, 1, with_alpha(0x000000, 35));
        let old = c.push_clip(area.inset(2));
        let crumbs = self.crumbs();
        let n = crumbs.len();
        for (i, (name, _, r)) in crumbs.iter().enumerate() {
            if self.hover == Hover::Crumb(i) {
                c.fill_rounded_rect(*r, 6, theme::HOVER);
            }
            let last = i + 1 == n;
            let font = if last { &f.bold } else { &f.ui };
            let col = if last { theme::TEXT } else { theme::TEXT_DIM };
            let base = r.y + (r.h + font.ascent - font.descent) / 2;
            c.draw_text(font, r.x + 6, base, name, col);
            if !last {
                c.draw_text(&f.ui, r.right() + 2, base, "\u{203a}", theme::TEXT_DIM);
            }
        }
        c.restore_clip(old);
        button(c, self.tool_rect(Tool::NewFolder), "New Folder", ButtonStyle::Normal, self.hover == Hover::Tool(Tool::NewFolder), true);
        button(c, self.tool_rect(Tool::NewFile), "New File", ButtonStyle::Primary, self.hover == Hover::Tool(Tool::NewFile), true);
    }

    fn draw_sidebar(&self, c: &mut Canvas) {
        let f = fonts();
        let h = self.size.1;
        let side = Rect::new(0, TOOLBAR_H, SIDEBAR_W, h - TOOLBAR_H);
        c.fill_rect(side, theme::SIDEBAR_BG);
        c.vline(SIDEBAR_W - 1, TOOLBAR_H, h - TOOLBAR_H, theme::SEPARATOR);
        c.draw_text(&f.small_bold, 18, TOOLBAR_H + 24, "PLACES", theme::TEXT_DIM);
        for (i, (label, path, icon)) in self.places().iter().enumerate() {
            let r = self.place_rect(i);
            let current = self.path == *path;
            if current {
                c.fill_rounded_rect(r, 7, with_alpha(0x000000, 22));
            } else if self.hover == Hover::Place(i) {
                c.fill_rounded_rect(r, 7, with_alpha(0x000000, 12));
            }
            icons::draw(c, *icon, r.x + 8, r.y + 4, 20);
            let base = r.y + (r.h + f.ui.ascent - f.ui.descent) / 2;
            let font = if current { &f.bold } else { &f.ui };
            c.draw_text(font, r.x + 36, base, label, theme::TEXT);
        }
        // Disk usage.
        if let Ok(s) = fs::stats() {
            let y = h - 74;
            c.draw_text(&f.small_bold, 18, y, "DISK", theme::TEXT_DIM);
            let bar = Rect::new(18, y + 10, SIDEBAR_W - 36, 8);
            c.fill_rounded_rect(bar, 4, with_alpha(0x000000, 30));
            let total = s.total_bytes().max(1);
            let used = total - s.free_bytes();
            let uw = ((bar.w as u64 * used / total) as i32).max(8);
            c.fill_rounded_rect(Rect::new(bar.x, bar.y, uw, bar.h), 4, theme::ACCENT);
            let text = format!("{} free", fs::format_size(s.free_bytes()));
            c.draw_text_clipped(&f.ui, 18, y + 38, &text, SIDEBAR_W - 30, theme::TEXT_DIM);
        }
    }

    fn columns(&self) -> (i32, Option<i32>, Option<i32>, Option<i32>) {
        let l = self.list_rect();
        let w = l.w;
        let kind = if w > 620 { Some(l.x + w - 150) } else { None };
        let modified = if w > 480 { Some(l.x + w - 150 - if kind.is_some() { 150 } else { 0 }) } else { None };
        let size_x = if w > 330 {
            Some(l.x + w - 150 - if kind.is_some() { 150 } else { 0 } - if modified.is_some() { 90 } else { 0 })
        } else {
            None
        };
        (l.x + 16, size_x, modified, kind)
    }

    fn draw_list(&mut self, c: &mut Canvas, focused: bool) {
        let f = fonts();
        let l = self.list_rect();
        c.fill_rect(Rect::new(l.x, l.y - HEADER_H, l.w, l.h + HEADER_H), theme::WINDOW_BG);
        // Header.
        let (name_x, size_x, mod_x, kind_x) = self.columns();
        let hy = l.y - HEADER_H;
        let hb = hy + (HEADER_H + f.small_bold.ascent - f.small_bold.descent) / 2;
        c.draw_text(&f.small_bold, name_x + 28, hb, "NAME", theme::TEXT_DIM);
        if let Some(x) = size_x {
            c.draw_text(&f.small_bold, x, hb, "SIZE", theme::TEXT_DIM);
        }
        if let Some(x) = mod_x {
            c.draw_text(&f.small_bold, x, hb, "MODIFIED", theme::TEXT_DIM);
        }
        if let Some(x) = kind_x {
            c.draw_text(&f.small_bold, x, hb, "KIND", theme::TEXT_DIM);
        }
        c.hline(l.x, l.y - 1, l.w, theme::SEPARATOR);

        let old = c.push_clip(l);
        if self.entries.is_empty() {
            let msg = "This folder is empty";
            c.draw_text_centered(&f.ui, Rect::new(l.x, l.y + 40, l.w, 30), msg, theme::TEXT_DIM);
            c.draw_text_centered(&f.ui, Rect::new(l.x, l.y + 64, l.w, 30), "Right-click to create a file or folder", with_alpha(theme::TEXT_DIM, 160));
        }
        let first = (self.scroll / ROW_H).max(0) as usize;
        let visible = (l.h / ROW_H + 2) as usize;
        for i in first..(first + visible).min(self.entries.len()) {
            let r = self.row_rect(i);
            let e = &self.entries[i];
            let selected = self.selected == Some(i);
            if selected {
                c.fill_rounded_rect(r, 7, if focused { theme::ACCENT } else { with_alpha(0x000000, 30) });
            } else if self.hover == Hover::Row(i) {
                c.fill_rounded_rect(r, 7, theme::HOVER);
            } else if i % 2 == 1 {
                c.fill_rounded_rect(r, 7, rgb(0xf8, 0xf9, 0xfb));
            }
            let (icon, kind) = kind_of(e, &self.path);
            icons::draw(c, icon, name_x - 4, r.y + 3, 22);
            let text = if selected && focused { theme::TEXT_ON_ACCENT } else { theme::TEXT };
            let dim = if selected && focused { with_alpha(0xffffff, 210) } else { theme::TEXT_DIM };
            let base = r.y + (r.h + f.ui.ascent - f.ui.descent) / 2;
            let name_w = size_x.unwrap_or(r.right() - 10) - name_x - 40;
            c.draw_text_clipped(&f.ui, name_x + 28, base, &e.name, name_w, text);
            if let Some(x) = size_x {
                let s = if e.is_dir { String::from("\u{2014}") } else { fs::format_size(e.size as u64) };
                c.draw_text(&f.ui, x, base, &s, dim);
            }
            if let Some(x) = mod_x {
                c.draw_text(&f.ui, x, base, &fs::format_time(&e.modified), dim);
            }
            if let Some(x) = kind_x {
                c.draw_text_clipped(&f.ui, x, base, &kind, 140, dim);
            }
        }
        widgets::draw_scrollbar(c, self.scrollbar_track(), self.content_height(), l.h, self.scroll);
        c.restore_clip(old);
    }

    fn draw_status(&self, c: &mut Canvas) {
        let f = fonts();
        let (w, h) = self.size;
        let r = Rect::new(SIDEBAR_W, h - STATUS_H, w - SIDEBAR_W, STATUS_H);
        c.fill_rect(r, theme::PANEL_BG);
        c.hline(r.x, r.y, r.w, theme::SEPARATOR);
        let base = r.y + (r.h + f.ui.ascent - f.ui.descent) / 2;
        if let Some((msg, error, _)) = &self.message {
            let col = if *error { theme::DANGER } else { theme::ACCENT_DARK };
            c.draw_text_clipped(&f.ui, r.x + 16, base, msg, r.w - 32, col);
            return;
        }
        let dirs = self.entries.iter().filter(|e| e.is_dir).count();
        let files = self.entries.len() - dirs;
        let mut text = format!(
            "{} folder{}, {} file{}",
            dirs,
            if dirs == 1 { "" } else { "s" },
            files,
            if files == 1 { "" } else { "s" }
        );
        if let Some(e) = self.selected.and_then(|i| self.entries.get(i)) {
            text = if e.is_dir {
                format!("\u{201c}{}\u{201d} selected", e.name)
            } else {
                format!("\u{201c}{}\u{201d} selected \u{2014} {}", e.name, fs::format_size(e.size as u64))
            };
        }
        c.draw_text_clipped(&f.ui, r.x + 16, base, &text, r.w - 32, theme::TEXT_DIM);
    }

    fn draw_menu(&self, c: &mut Canvas) {
        let Some(m) = &self.menu else { return };
        let f = fonts();
        let r = m.rect(self.size);
        c.draw_shadow(r, 9, 14, with_alpha(0x000000, 70));
        c.fill_rounded_rect(r, 9, rgb(0xfb, 0xfb, 0xfd));
        c.stroke_rounded_rect(r, 9, 1, with_alpha(0x000000, 40));
        let mut y = r.y + 5;
        for (i, it) in m.items.iter().enumerate() {
            match it {
                Some((a, label)) => {
                    let row = Rect::new(r.x + 5, y, r.w - 10, 26);
                    let hovered = self.hover == Hover::Menu(i);
                    if hovered {
                        c.fill_rounded_rect(row, 6, if *a == Action::Delete { theme::DANGER } else { theme::ACCENT });
                    }
                    let col = if hovered {
                        rgb(255, 255, 255)
                    } else if *a == Action::Delete {
                        theme::DANGER
                    } else {
                        theme::TEXT
                    };
                    let base = row.y + (row.h + f.ui.ascent - f.ui.descent) / 2;
                    c.draw_text(&f.ui, row.x + 12, base, label, col);
                    y += 26;
                }
                None => {
                    c.hline(r.x + 10, y + 4, r.w - 20, theme::SEPARATOR);
                    y += 9;
                }
            }
        }
    }
}

/// Hooks for the boot-time self-test.
impl Explorer {
    pub fn test_dialog_result(&mut self, tag: u32, value: Option<String>) {
        self.dialog_result(tag, value);
    }

    pub fn test_select(&mut self, name: &str) {
        self.refresh();
        self.selected = self.entries.iter().position(|e| e.name == name);
    }

    pub fn test_rename(&mut self, new_name: &str) {
        self.pending = self.selected_path();
        self.dialog_result(TAG_RENAME, Some(String::from(new_name)));
    }

    pub fn test_delete(&mut self) {
        self.pending = self.selected_path();
        self.dialog_result(TAG_DELETE, Some(String::from("ok")));
    }
}

impl App for Explorer {
    fn title(&self) -> String {
        if self.path == "/" { String::from("MayOS Disk") } else { String::from(fs::file_name(&self.path)) }
    }
    fn icon(&self) -> Icon {
        Icon::Explorer
    }
    fn kind(&self) -> AppKind {
        AppKind::Explorer
    }
    fn initial_size(&self) -> (i32, i32) {
        (780, 480)
    }
    fn min_size(&self) -> (i32, i32) {
        (520, 280)
    }

    fn render(&mut self, c: &mut Canvas, size: (i32, i32), focused: bool) {
        self.size = size;
        self.focused = focused;
        self.clamp_scroll();
        self.draw_toolbar(c);
        self.draw_sidebar(c);
        self.draw_list(c, focused);
        self.draw_status(c);
        self.draw_menu(c);
    }

    fn event(&mut self, ev: &AppEvent, ctx: &mut Ctx) {
        match ev {
            AppEvent::MouseMove { x, y, .. } => {
                if self.dragging_scrollbar {
                    let track = self.scrollbar_track();
                    let range = (self.content_height() - self.list_rect().h).max(0);
                    self.scroll = ((y - track.y) * range / track.h.max(1)).clamp(0, range);
                    ctx.redraw();
                    return;
                }
                let h = self.hit(*x, *y);
                if h != self.hover {
                    self.hover = h;
                    ctx.redraw();
                }
            }
            AppEvent::MouseLeave => {
                if self.hover != Hover::None {
                    self.hover = Hover::None;
                    ctx.redraw();
                }
            }
            AppEvent::MouseUp { .. } => self.dragging_scrollbar = false,
            AppEvent::MouseDown { x, y, button, clicks } => {
                let (x, y) = (*x, *y);
                ctx.redraw();
                if let Some(m) = self.menu.take() {
                    if let Some(i) = m.item_at(self.size, x, y)
                        && let Some((a, _)) = m.items[i]
                    {
                        self.run(a, ctx);
                    }
                    self.hover = self.hit(x, y);
                    return;
                }
                if *button == 0 && self.scrollbar_track().inset(-3).contains(x, y) && self.content_height() > self.list_rect().h {
                    self.dragging_scrollbar = true;
                    return;
                }
                match self.hit(x, y) {
                    Hover::Tool(t) if *button == 0 => match t {
                        Tool::Back => self.go_back(),
                        Tool::Forward => self.go_forward(),
                        Tool::Up => self.go_up(),
                        Tool::NewFolder => self.run(Action::NewFolder, ctx),
                        Tool::NewFile => self.run(Action::NewFile, ctx),
                    },
                    Hover::Crumb(i) if *button == 0 => {
                        if let Some((_, path, _)) = self.crumbs().get(i) {
                            let p = path.clone();
                            self.navigate(&p, true);
                        }
                    }
                    Hover::Place(i) if *button == 0 => {
                        if let Some((_, path, _)) = self.places().get(i) {
                            self.navigate(path, true);
                        }
                    }
                    Hover::Row(i) => {
                        self.selected = Some(i);
                        if *button == 1 {
                            self.open_menu(x, y, true);
                        } else if *clicks >= 2 {
                            self.open_entry(i, ctx);
                        }
                    }
                    _ => {
                        if self.list_rect().contains(x, y) {
                            self.selected = None;
                            if *button == 1 {
                                self.open_menu(x, y, false);
                            }
                        }
                    }
                }
            }
            AppEvent::Wheel { delta, .. } => {
                if self.menu.is_none() {
                    self.scroll += delta * ROW_H * 2;
                    self.clamp_scroll();
                    ctx.redraw();
                }
            }
            AppEvent::Key(k) if k.pressed => {
                if self.menu.is_some() {
                    if k.key == Key::Escape {
                        self.menu = None;
                        ctx.redraw();
                    }
                    return;
                }
                let n = self.entries.len();
                match k.key {
                    Key::Down if n > 0 => {
                        let i = self.selected.map(|i| (i + 1).min(n - 1)).unwrap_or(0);
                        self.selected = Some(i);
                        self.ensure_visible(i);
                    }
                    Key::Up if n > 0 => {
                        let i = self.selected.map(|i| i.saturating_sub(1)).unwrap_or(0);
                        self.selected = Some(i);
                        self.ensure_visible(i);
                    }
                    Key::Home if n > 0 => {
                        self.selected = Some(0);
                        self.ensure_visible(0);
                    }
                    Key::End if n > 0 => {
                        self.selected = Some(n - 1);
                        self.ensure_visible(n - 1);
                    }
                    Key::Enter => self.run(Action::Open, ctx),
                    Key::Backspace => self.go_up(),
                    Key::Left if k.alt => self.go_back(),
                    Key::Right if k.alt => self.go_forward(),
                    Key::Delete => self.run(Action::Delete, ctx),
                    Key::F(2) => self.run(Action::Rename, ctx),
                    Key::F(5) => self.refresh(),
                    Key::Char('c') if k.ctrl => self.run(Action::Copy, ctx),
                    Key::Char('x') if k.ctrl => self.run(Action::Cut, ctx),
                    Key::Char('v') if k.ctrl => self.run(Action::Paste, ctx),
                    Key::Char('d') if k.ctrl => self.run(Action::Duplicate, ctx),
                    Key::Char('N') if k.ctrl => self.run(Action::NewFolder, ctx),
                    Key::Char('n') if k.ctrl => self.run(Action::NewFile, ctx),
                    Key::Char(ch) if !k.ctrl && !k.alt && n > 0 => {
                        // Type-to-select: jump to the next name starting with ch.
                        let lower = ch.to_ascii_lowercase();
                        let start = self.selected.map(|i| i + 1).unwrap_or(0);
                        let found = (0..n)
                            .map(|k| (start + k) % n)
                            .find(|&i| self.entries[i].name.chars().next().map(|c| c.to_ascii_lowercase()) == Some(lower));
                        if let Some(i) = found {
                            self.selected = Some(i);
                            self.ensure_visible(i);
                        }
                    }
                    _ => return,
                }
                ctx.redraw();
            }
            AppEvent::Message(Msg::DialogResult { tag, value }) => {
                self.dialog_result(*tag, value.clone());
                ctx.redraw();
            }
            AppEvent::Focus(_) | AppEvent::Resized { .. } => ctx.redraw(),
            _ => {}
        }
    }

    fn tick(&mut self, ctx: &mut Ctx) {
        if fs::generation() != self.generation {
            self.refresh();
            ctx.redraw();
        }
        if let Some((_, _, until)) = &self.message
            && crate::time::uptime_ms() > *until
        {
            self.message = None;
            ctx.redraw();
        }
    }
}
