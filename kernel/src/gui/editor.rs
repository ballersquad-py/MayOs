//! Text editor with selection, clipboard, save / save as, and a prompt for
//! unsaved changes.

use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use gfx::icons::Icon;
use gfx::{rgb, with_alpha, Canvas, Rect};

use super::app::{App, AppEvent, AppKind, Ctx, Msg};
use super::dialog::Dialog;
use super::theme::{self, fonts};
use super::widgets::{self, button, ButtonStyle};
use crate::fs;
use crate::input::Key;
use crate::sync::Spin;

const TOOLBAR_H: i32 = 44;
const STATUS_H: i32 = 24;
const GUTTER: i32 = 52;
const PAD: i32 = 8;

const TAG_SAVE_AS: u32 = 10;
const TAG_UNSAVED: u32 = 11;

static CLIPBOARD: Spin<String> = Spin::new(String::new());

type Pos = (usize, usize);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Hover {
    None,
    Save,
    SaveAs,
}

pub struct Editor {
    path: Option<String>,
    lines: Vec<String>,
    cursor: Pos,
    anchor: Option<Pos>,
    want_col: usize,
    top: usize,
    left: i32,
    modified: bool,
    size: (i32, i32),
    message: Option<(String, bool, u64)>,
    close_after_save: bool,
    hover: Hover,
    focused: bool,
    blink_on: bool,
    last_blink: u64,
    selecting: bool,
}

fn char_len(s: &str) -> usize {
    s.chars().count()
}

fn byte_at(s: &str, ch: usize) -> usize {
    s.char_indices().nth(ch).map(|(i, _)| i).unwrap_or(s.len())
}

impl Editor {
    pub fn new_empty() -> Editor {
        Editor {
            path: None,
            lines: alloc::vec![String::new()],
            cursor: (0, 0),
            anchor: None,
            want_col: 0,
            top: 0,
            left: 0,
            modified: false,
            size: (700, 460),
            message: None,
            close_after_save: false,
            hover: Hover::None,
            focused: true,
            blink_on: true,
            last_blink: 0,
            selecting: false,
        }
    }

    pub fn open(path: &str) -> Editor {
        let mut e = Editor::new_empty();
        e.path = Some(String::from(path));
        match fs::read_file(path) {
            Ok(data) => {
                let binary = data.iter().take(4096).any(|&b| b == 0);
                let text = String::from_utf8_lossy(&data);
                e.lines = text.split('\n').map(|l| l.trim_end_matches('\r').to_string()).collect();
                if e.lines.is_empty() {
                    e.lines.push(String::new());
                }
                if binary {
                    e.flash("This looks like a binary file; saving may damage it.".into(), true);
                }
            }
            Err(fs::FsError::NotFound) => e.flash("New file \u{2014} it will be created when you save.".into(), false),
            Err(err) => e.flash(format!("Cannot read file: {}", err), true),
        }
        e
    }

    fn flash(&mut self, msg: String, error: bool) {
        self.message = Some((msg, error, crate::time::uptime_ms() + 5000));
    }

    fn name(&self) -> String {
        match &self.path {
            Some(p) => String::from(fs::file_name(p)),
            None => String::from("Untitled"),
        }
    }

    fn text(&self) -> String {
        self.lines.join("\n")
    }

    fn save_to(&mut self, path: &str) -> bool {
        let data = self.text();
        match fs::write_file(path, data.as_bytes()) {
            Ok(()) => {
                self.path = Some(String::from(path));
                self.modified = false;
                crate::audio::play_system(crate::audio::SystemSound::Notify);
                self.flash(format!("Saved to {}", path), false);
                true
            }
            Err(e) => {
                self.flash(format!("Save failed: {}", e), true);
                false
            }
        }
    }

    fn save(&mut self, ctx: &mut Ctx) {
        match self.path.clone() {
            Some(p) => {
                if self.save_to(&p) && self.close_after_save {
                    ctx.close();
                }
            }
            None => self.save_as(ctx),
        }
    }

    fn save_as(&mut self, ctx: &mut Ctx) {
        let initial = self.path.clone().unwrap_or_else(|| {
            let dir = if fs::is_dir("/docs") { "/docs" } else { "/" };
            fs::join(dir, &fs::unique_name(dir, "Untitled.txt"))
        });
        ctx.open_child(Box::new(Dialog::input("Save As", "Save the file as (full path):", &initial, "Save", TAG_SAVE_AS, ctx.window)));
    }

    // ---------------------------------------------------------------
    // Geometry
    // ---------------------------------------------------------------

    fn cw(&self) -> i32 {
        (fonts().mono.advance16('M') + 8) / 16
    }

    fn lh(&self) -> i32 {
        fonts().mono.line_height + 2
    }

    fn text_rect(&self) -> Rect {
        let (w, h) = self.size;
        Rect::new(0, TOOLBAR_H, w, h - TOOLBAR_H - STATUS_H)
    }

    fn visible_lines(&self) -> usize {
        (self.text_rect().h / self.lh()).max(1) as usize
    }

    fn save_rect(&self) -> Rect {
        Rect::new(12, 7, 70, 30)
    }

    fn save_as_rect(&self) -> Rect {
        Rect::new(88, 7, 86, 30)
    }

    fn pos_at(&self, x: i32, y: i32) -> Pos {
        let t = self.text_rect();
        let row = ((y - t.y - PAD).max(0) / self.lh()) as usize + self.top;
        let line = row.min(self.lines.len() - 1);
        let col = ((x - t.x - GUTTER - PAD + self.left + self.cw() / 2).max(0) / self.cw()) as usize;
        (line, col.min(char_len(&self.lines[line])))
    }

    fn scroll_to_cursor(&mut self) {
        let vis = self.visible_lines();
        if self.cursor.0 < self.top {
            self.top = self.cursor.0;
        } else if self.cursor.0 >= self.top + vis {
            self.top = self.cursor.0 + 1 - vis;
        }
        let x = self.cursor.1 as i32 * self.cw();
        let tw = self.text_rect().w - GUTTER - PAD * 2 - 12;
        if x < self.left {
            self.left = (x - tw / 3).max(0);
        } else if x > self.left + tw {
            self.left = x - tw + tw / 3;
        }
    }

    // ---------------------------------------------------------------
    // Editing
    // ---------------------------------------------------------------

    fn selection(&self) -> Option<(Pos, Pos)> {
        let a = self.anchor?;
        if a == self.cursor {
            return None;
        }
        Some(if a < self.cursor { (a, self.cursor) } else { (self.cursor, a) })
    }

    fn selected_text(&self) -> Option<String> {
        let (s, e) = self.selection()?;
        if s.0 == e.0 {
            let l = &self.lines[s.0];
            return Some(l[byte_at(l, s.1)..byte_at(l, e.1)].to_string());
        }
        let mut out = String::new();
        let first = &self.lines[s.0];
        out.push_str(&first[byte_at(first, s.1)..]);
        for l in &self.lines[s.0 + 1..e.0] {
            out.push('\n');
            out.push_str(l);
        }
        out.push('\n');
        let last = &self.lines[e.0];
        out.push_str(&last[..byte_at(last, e.1)]);
        Some(out)
    }

    fn delete_selection(&mut self) -> bool {
        let Some((s, e)) = self.selection() else {
            self.anchor = None;
            return false;
        };
        let tail = {
            let last = &self.lines[e.0];
            last[byte_at(last, e.1)..].to_string()
        };
        let first = &mut self.lines[s.0];
        let cut = byte_at(first, s.1);
        first.truncate(cut);
        first.push_str(&tail);
        self.lines.drain(s.0 + 1..=e.0);
        self.cursor = s;
        self.anchor = None;
        self.modified = true;
        true
    }

    fn insert_text(&mut self, text: &str) {
        self.delete_selection();
        for (i, part) in text.split('\n').enumerate() {
            if i > 0 {
                self.newline();
            }
            let line = &mut self.lines[self.cursor.0];
            let at = byte_at(line, self.cursor.1);
            let part = part.trim_end_matches('\r');
            line.insert_str(at, part);
            self.cursor.1 += char_len(part);
        }
        self.modified = true;
    }

    fn newline(&mut self) {
        let line = &mut self.lines[self.cursor.0];
        let at = byte_at(line, self.cursor.1);
        let rest = line.split_off(at);
        // Keep the indentation of the current line.
        let indent: String = line.chars().take_while(|c| *c == ' ').collect();
        let n = char_len(&indent);
        self.lines.insert(self.cursor.0 + 1, indent + &rest);
        self.cursor = (self.cursor.0 + 1, n);
        self.modified = true;
    }

    fn backspace(&mut self) {
        if self.delete_selection() {
            return;
        }
        let (l, c) = self.cursor;
        if c > 0 {
            let line = &mut self.lines[l];
            let at = byte_at(line, c - 1);
            line.remove(at);
            self.cursor.1 -= 1;
        } else if l > 0 {
            let cur = self.lines.remove(l);
            let prev = &mut self.lines[l - 1];
            let n = char_len(prev);
            prev.push_str(&cur);
            self.cursor = (l - 1, n);
        } else {
            return;
        }
        self.modified = true;
    }

    fn delete(&mut self) {
        if self.delete_selection() {
            return;
        }
        let (l, c) = self.cursor;
        if c < char_len(&self.lines[l]) {
            let line = &mut self.lines[l];
            let at = byte_at(line, c);
            line.remove(at);
        } else if l + 1 < self.lines.len() {
            let next = self.lines.remove(l + 1);
            self.lines[l].push_str(&next);
        } else {
            return;
        }
        self.modified = true;
    }

    fn move_cursor(&mut self, to: Pos, extend: bool, keep_col: bool) {
        if extend {
            if self.anchor.is_none() {
                self.anchor = Some(self.cursor);
            }
        } else {
            self.anchor = None;
        }
        self.cursor = to;
        if !keep_col {
            self.want_col = to.1;
        }
    }

    fn key(&mut self, k: &crate::input::KeyEvent, ctx: &mut Ctx) {
        let (l, c) = self.cursor;
        let len = char_len(&self.lines[l]);
        let shift = k.shift;
        match k.key {
            Key::Char('s') if k.ctrl => self.save(ctx),
            Key::Char('S') if k.ctrl => self.save_as(ctx),
            Key::Char('a') if k.ctrl => {
                self.anchor = Some((0, 0));
                let last = self.lines.len() - 1;
                self.cursor = (last, char_len(&self.lines[last]));
            }
            Key::Char('c') if k.ctrl => {
                if let Some(t) = self.selected_text() {
                    *CLIPBOARD.lock() = t;
                    self.flash("Copied".into(), false);
                }
            }
            Key::Char('x') if k.ctrl => {
                if let Some(t) = self.selected_text() {
                    *CLIPBOARD.lock() = t;
                    self.delete_selection();
                }
            }
            Key::Char('v') if k.ctrl => {
                let t = CLIPBOARD.lock().clone();
                if !t.is_empty() {
                    self.insert_text(&t);
                }
            }
            Key::Enter => {
                self.delete_selection();
                self.newline();
            }
            Key::Backspace => self.backspace(),
            Key::Delete => self.delete(),
            Key::Tab => self.insert_text("    "),
            Key::Left => {
                if !shift && let Some((s, _)) = self.selection() {
                    self.move_cursor(s, false, false);
                } else if c > 0 {
                    self.move_cursor((l, c - 1), shift, false);
                } else if l > 0 {
                    self.move_cursor((l - 1, char_len(&self.lines[l - 1])), shift, false);
                }
            }
            Key::Right => {
                if !shift && let Some((_, e)) = self.selection() {
                    self.move_cursor(e, false, false);
                } else if c < len {
                    self.move_cursor((l, c + 1), shift, false);
                } else if l + 1 < self.lines.len() {
                    self.move_cursor((l + 1, 0), shift, false);
                }
            }
            Key::Up => {
                if l > 0 {
                    let col = self.want_col.min(char_len(&self.lines[l - 1]));
                    self.move_cursor((l - 1, col), shift, true);
                }
            }
            Key::Down => {
                if l + 1 < self.lines.len() {
                    let col = self.want_col.min(char_len(&self.lines[l + 1]));
                    self.move_cursor((l + 1, col), shift, true);
                }
            }
            Key::Home => self.move_cursor((l, 0), shift, false),
            Key::End => self.move_cursor((l, len), shift, false),
            Key::PageUp => {
                let nl = l.saturating_sub(self.visible_lines());
                let col = self.want_col.min(char_len(&self.lines[nl]));
                self.move_cursor((nl, col), shift, true);
            }
            Key::PageDown => {
                let nl = (l + self.visible_lines()).min(self.lines.len() - 1);
                let col = self.want_col.min(char_len(&self.lines[nl]));
                self.move_cursor((nl, col), shift, true);
            }
            _ => {
                if let Some(ch) = k.text() {
                    let mut buf = [0u8; 4];
                    self.insert_text(ch.encode_utf8(&mut buf));
                } else {
                    return;
                }
            }
        }
        self.want_col = if matches!(k.key, Key::Up | Key::Down | Key::PageUp | Key::PageDown) {
            self.want_col
        } else {
            self.cursor.1
        };
        self.scroll_to_cursor();
    }
}

impl App for Editor {
    fn title(&self) -> String {
        format!("{}{} \u{2014} Text Editor", if self.modified { "\u{2022} " } else { "" }, self.name())
    }
    fn icon(&self) -> Icon {
        Icon::Editor
    }
    fn kind(&self) -> AppKind {
        AppKind::Editor
    }
    fn initial_size(&self) -> (i32, i32) {
        (720, 480)
    }
    fn min_size(&self) -> (i32, i32) {
        (360, 200)
    }

    fn render(&mut self, c: &mut Canvas, size: (i32, i32), focused: bool) {
        self.size = size;
        self.focused = focused;
        let (w, h) = size;
        let f = fonts();
        // Toolbar.
        c.fill_rect(Rect::new(0, 0, w, TOOLBAR_H), theme::PANEL_BG);
        c.hline(0, TOOLBAR_H - 1, w, theme::SEPARATOR);
        button(c, self.save_rect(), "Save", ButtonStyle::Primary, self.hover == Hover::Save, true);
        button(c, self.save_as_rect(), "Save As\u{2026}", ButtonStyle::Normal, self.hover == Hover::SaveAs, true);
        let path = self.path.clone().unwrap_or_else(|| String::from("(not saved yet)"));
        let base = (TOOLBAR_H + f.ui.ascent - f.ui.descent) / 2;
        c.draw_text_clipped(&f.ui, 190, base, &path, w - 320, theme::TEXT_DIM);
        let pos = format!("Ln {}, Col {}", self.cursor.0 + 1, self.cursor.1 + 1);
        let pw = f.ui.measure(&pos);
        c.draw_text(&f.ui, w - pw - 14, base, &pos, theme::TEXT_DIM);

        // Text area.
        let t = self.text_rect();
        c.fill_rect(t, theme::WINDOW_BG);
        c.fill_rect(Rect::new(0, t.y, GUTTER, t.h), rgb(0xf6, 0xf7, 0xf9));
        c.vline(GUTTER - 1, t.y, t.h, theme::SEPARATOR);
        let lh = self.lh();
        let cw = self.cw();
        let vis = self.visible_lines();
        let sel = self.selection();
        let text_x = GUTTER + PAD - self.left;
        for row in 0..=vis {
            let li = self.top + row;
            if li >= self.lines.len() {
                break;
            }
            let y = t.y + PAD + row as i32 * lh;
            if li == self.cursor.0 && sel.is_none() {
                c.fill_rect(Rect::new(GUTTER, y - 1, t.w - GUTTER, lh), rgb(0xf3, 0xf7, 0xff));
            }
            let num = format!("{}", li + 1);
            let nw = f.mono.measure(&num);
            let ncol = if li == self.cursor.0 { theme::TEXT } else { with_alpha(theme::TEXT_DIM, 170) };
            c.draw_text(&f.mono, GUTTER - 10 - nw, y + f.mono.ascent, &num, ncol);
            let old = c.push_clip(Rect::new(GUTTER, t.y, t.w - GUTTER, t.h));
            let line = &self.lines[li];
            if let Some((s, e)) = sel
                && li >= s.0
                && li <= e.0
            {
                let from = if li == s.0 { s.1 } else { 0 };
                let to = if li == e.0 { e.1 } else { char_len(line) + 1 };
                let sel_col = if focused { theme::selection() } else { rgb(0xe6, 0xe8, 0xec) };
                c.fill_rect(Rect::new(text_x + from as i32 * cw, y - 1, (to - from) as i32 * cw, lh), sel_col);
            }
            c.draw_text(&f.mono, text_x, y + f.mono.ascent, line, theme::TEXT);
            if li == self.cursor.0 && focused && self.blink_on {
                c.fill_rect(Rect::new(text_x + self.cursor.1 as i32 * cw, y - 1, 2, lh), theme::accent());
            }
            c.restore_clip(old);
        }
        let total = self.lines.len() as i32 * lh;
        widgets::draw_scrollbar(c, Rect::new(w - 11, t.y + 2, 9, t.h - 4), total + 2 * PAD, t.h, self.top as i32 * lh);

        // Status bar.
        let s = Rect::new(0, h - STATUS_H, w, STATUS_H);
        c.fill_rect(s, theme::PANEL_BG);
        c.hline(0, s.y, w, theme::SEPARATOR);
        let sb = s.y + (s.h + f.ui.ascent - f.ui.descent) / 2;
        let (msg, col) = match &self.message {
            Some((m, err, _)) => (m.clone(), if *err { theme::DANGER } else { theme::accent_dark() }),
            None => (
                format!(
                    "{} lines \u{00b7} {}{}",
                    self.lines.len(),
                    fs::format_size(self.text().len() as u64),
                    if self.modified { " \u{00b7} unsaved changes" } else { "" }
                ),
                theme::TEXT_DIM,
            ),
        };
        c.draw_text_clipped(&f.ui, 12, sb, &msg, w - 24, col);
    }

    fn event(&mut self, ev: &AppEvent, ctx: &mut Ctx) {
        match ev {
            AppEvent::Key(k) if k.pressed => {
                self.blink_on = true;
                self.last_blink = crate::time::uptime_ms();
                self.key(k, ctx);
                ctx.redraw();
            }
            AppEvent::MouseMove { x, y, buttons } => {
                if self.selecting && buttons & 1 != 0 {
                    let p = self.pos_at(*x, *y);
                    if p != self.cursor {
                        self.cursor = p;
                        self.scroll_to_cursor();
                        ctx.redraw();
                    }
                    return;
                }
                let h = if self.save_rect().contains(*x, *y) {
                    Hover::Save
                } else if self.save_as_rect().contains(*x, *y) {
                    Hover::SaveAs
                } else {
                    Hover::None
                };
                if h != self.hover {
                    self.hover = h;
                    ctx.redraw();
                }
            }
            AppEvent::MouseDown { x, y, button: 0, clicks } => {
                if self.save_rect().contains(*x, *y) {
                    self.save(ctx);
                } else if self.save_as_rect().contains(*x, *y) {
                    self.save_as(ctx);
                } else if self.text_rect().contains(*x, *y) {
                    let p = self.pos_at(*x, *y);
                    if *clicks >= 2 {
                        // Select the word under the pointer.
                        let line: Vec<char> = self.lines[p.0].chars().collect();
                        let is_word = |c: char| c.is_alphanumeric() || c == '_';
                        let mut s = p.1;
                        while s > 0 && line.get(s - 1).copied().map(is_word).unwrap_or(false) {
                            s -= 1;
                        }
                        let mut e = p.1;
                        while e < line.len() && is_word(line[e]) {
                            e += 1;
                        }
                        self.anchor = Some((p.0, s));
                        self.cursor = (p.0, e);
                    } else {
                        self.anchor = Some(p);
                        self.cursor = p;
                        self.selecting = true;
                    }
                    self.want_col = self.cursor.1;
                }
                ctx.redraw();
            }
            AppEvent::MouseUp { .. } => self.selecting = false,
            AppEvent::Wheel { delta, .. } => {
                let max = self.lines.len().saturating_sub(1);
                let top = self.top as i32 + delta * 3;
                self.top = top.clamp(0, max as i32) as usize;
                ctx.redraw();
            }
            AppEvent::Message(Msg::DialogResult { tag, value }) => {
                match (*tag, value.as_deref()) {
                    (TAG_SAVE_AS, Some(p)) => {
                        let p = fs::normalize("/", p.trim());
                        if self.save_to(&p) && self.close_after_save {
                            ctx.close();
                        }
                    }
                    (TAG_UNSAVED, Some("ok")) => {
                        self.close_after_save = true;
                        self.save(ctx);
                    }
                    (TAG_UNSAVED, Some("alt")) => {
                        self.modified = false;
                        ctx.close();
                    }
                    _ => self.close_after_save = false,
                }
                ctx.redraw();
            }
            AppEvent::Focus(_) | AppEvent::Resized { .. } => ctx.redraw(),
            _ => {}
        }
    }

    fn tick(&mut self, ctx: &mut Ctx) {
        let now = crate::time::uptime_ms();
        if self.focused && now - self.last_blink >= 530 {
            self.last_blink = now;
            self.blink_on = !self.blink_on;
            ctx.redraw();
        }
        if let Some((_, _, until)) = &self.message
            && now > *until
        {
            self.message = None;
            ctx.redraw();
        }
    }

    fn request_close(&mut self, ctx: &mut Ctx) -> bool {
        if !self.modified {
            return true;
        }
        ctx.open_child(Box::new(Dialog::choice(
            "Unsaved Changes",
            &format!("Save changes to \u{201c}{}\u{201d} before closing?", self.name()),
            "Save",
            "Don't Save",
            TAG_UNSAVED,
            ctx.window,
        )));
        false
    }
}
