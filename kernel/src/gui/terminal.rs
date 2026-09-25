//! Terminal emulator window: runs the built-in shell and user programs.

use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use gfx::icons::Icon;
use gfx::{rgb, Canvas, Color, Rect};

use super::app::{App, AppEvent, AppKind, Ctx};
use super::theme::fonts;
use super::widgets;
use crate::input::Key;
use crate::proc::process::{self, Console, Process};

pub const BG: Color = rgb(0x1c, 0x1f, 0x26);
pub const FG: Color = rgb(0xdc, 0xdf, 0xe4);
pub const DIM: Color = rgb(0x8b, 0x92, 0xa0);
pub const BLUE: Color = rgb(0x61, 0xaf, 0xef);
pub const GREEN: Color = rgb(0x98, 0xc3, 0x79);
pub const RED: Color = rgb(0xe0, 0x6c, 0x75);
pub const YELLOW: Color = rgb(0xe5, 0xc0, 0x7b);
pub const CYAN: Color = rgb(0x56, 0xb6, 0xc2);
pub const MAGENTA: Color = rgb(0xc6, 0x78, 0xdd);

const PALETTE: [Color; 8] = [rgb(0x5c, 0x63, 0x70), RED, GREEN, YELLOW, BLUE, MAGENTA, CYAN, FG];
const MAX_LINES: usize = 3000;
const PAD: i32 = 10;

type Line = Vec<(char, Color)>;

enum Ansi {
    Normal,
    Escape,
    Csi(String),
}

pub struct Terminal {
    lines: Vec<Line>,
    color: Color,
    ansi: Ansi,
    pub(super) cwd: String,
    input: String,
    caret: usize,
    history: Vec<String>,
    history_pos: Option<usize>,
    process: Option<Arc<Process>>,
    console: Option<Arc<Console>>,
    proc_input: String,
    scroll: i32,
    cols: usize,
    rows: i32,
    size: (i32, i32),
    startup: Option<String>,
    focused: bool,
    blink_on: bool,
    last_blink: u64,
    pub(super) close_when_done: bool,
    dragging_scrollbar: bool,
}

impl Terminal {
    pub fn new() -> Terminal {
        let mut t = Terminal {
            lines: alloc::vec![Vec::new()],
            color: FG,
            ansi: Ansi::Normal,
            cwd: String::from("/"),
            input: String::new(),
            caret: 0,
            history: Vec::new(),
            history_pos: None,
            process: None,
            console: None,
            proc_input: String::new(),
            scroll: 0,
            cols: 80,
            rows: 24,
            size: (0, 0),
            startup: None,
            focused: true,
            blink_on: true,
            last_blink: 0,
            close_when_done: false,
            dragging_scrollbar: false,
        };
        t.color = BLUE;
        t.print("MayOS Terminal\n");
        t.color = DIM;
        t.print("Type 'help' for a list of commands.\n\n");
        t.color = FG;
        t
    }

    /// A terminal that immediately runs `command` (e.g. a program opened
    /// from the file explorer).
    pub fn with_command(cwd: &str, command: &str) -> Terminal {
        let mut t = Terminal::new();
        t.cwd = String::from(cwd);
        t.startup = Some(String::from(command));
        t
    }

    fn char_w(&self) -> i32 {
        (fonts().mono.advance16('M') + 8) / 16
    }

    fn line_h(&self) -> i32 {
        fonts().mono.line_height + 1
    }

    pub fn set_color(&mut self, c: Color) {
        self.color = c;
    }

    /// Append text, interpreting newlines, backspace, tabs and ANSI colour
    /// escapes (SGR 0, 1, 30-37, 90-97) plus "clear screen".
    pub fn print(&mut self, s: &str) {
        for ch in s.chars() {
            match core::mem::replace(&mut self.ansi, Ansi::Normal) {
                Ansi::Escape => {
                    self.ansi = if ch == '[' { Ansi::Csi(String::new()) } else { Ansi::Normal };
                    continue;
                }
                Ansi::Csi(mut params) => {
                    if ch.is_ascii_digit() || ch == ';' {
                        params.push(ch);
                        self.ansi = Ansi::Csi(params);
                    } else {
                        self.csi(ch, &params);
                    }
                    continue;
                }
                Ansi::Normal => {}
            }
            match ch {
                '\x1b' => self.ansi = Ansi::Escape,
                '\n' => self.newline(),
                '\r' => {}
                '\x08' => {
                    self.lines.last_mut().unwrap().pop();
                }
                '\t' => {
                    let n = 4 - self.lines.last().unwrap().len() % 4;
                    for _ in 0..n {
                        self.put(' ');
                    }
                }
                c if (c as u32) < 0x20 => {}
                c => self.put(c),
            }
        }
        self.scroll = 0;
    }

    fn csi(&mut self, cmd: char, params: &str) {
        match cmd {
            'm' => {
                for p in params.split(';') {
                    let n: u32 = p.parse().unwrap_or(0);
                    match n {
                        0 => self.color = FG,
                        30..=37 => self.color = PALETTE[(n - 30) as usize],
                        90..=97 => self.color = PALETTE[(n - 90) as usize],
                        _ => {}
                    }
                }
            }
            'J' if params == "2" => self.clear(),
            _ => {}
        }
    }

    fn put(&mut self, c: char) {
        if self.lines.last().unwrap().len() >= self.cols.max(10) {
            self.newline();
        }
        let color = self.color;
        self.lines.last_mut().unwrap().push((c, color));
    }

    fn newline(&mut self) {
        self.lines.push(Vec::new());
        if self.lines.len() > MAX_LINES {
            let excess = self.lines.len() - MAX_LINES;
            self.lines.drain(..excess);
        }
    }

    pub fn clear(&mut self) {
        self.lines.clear();
        self.lines.push(Vec::new());
        self.scroll = 0;
    }

    fn prompt(&self) -> Vec<(char, Color)> {
        let mut v = Vec::new();
        for c in "user@mayos".chars() {
            v.push((c, GREEN));
        }
        v.push((':', FG));
        for c in self.cwd.chars() {
            v.push((c, BLUE));
        }
        for c in "$ ".chars() {
            v.push((c, FG));
        }
        v
    }

    /// The editable line shown after the scrollback.
    fn live_line(&self) -> (Line, usize) {
        if self.process.is_some() {
            let mut l = self.lines.last().cloned().unwrap_or_default();
            let start = l.len();
            for c in self.proc_input.chars() {
                l.push((c, FG));
            }
            return (l, start + self.proc_input.chars().count());
        }
        let mut l = self.prompt();
        let start = l.len();
        for c in self.input.chars() {
            l.push((c, FG));
        }
        (l, start + self.caret)
    }

    /// All visual rows, wrapped to the terminal width, plus the caret row
    /// and column.
    fn visual_rows(&self) -> (Vec<Line>, (usize, usize)) {
        let cols = self.cols.max(10);
        let mut rows: Vec<Line> = Vec::new();
        let body = if self.process.is_some() { &self.lines[..self.lines.len() - 1] } else { &self.lines[..] };
        // When no program is running the last scrollback line is empty
        // (output always ends with a newline before the prompt).
        let body = if self.process.is_none() && body.last().map(|l| l.is_empty()).unwrap_or(false) {
            &body[..body.len() - 1]
        } else {
            body
        };
        for l in body {
            if l.is_empty() {
                rows.push(Vec::new());
            }
            for chunk in l.chunks(cols) {
                rows.push(chunk.to_vec());
            }
        }
        let (live, caret) = self.live_line();
        let first = rows.len();
        if live.is_empty() {
            rows.push(Vec::new());
        }
        for chunk in live.chunks(cols) {
            rows.push(chunk.to_vec());
        }
        if caret > 0 && caret % cols == 0 && caret == live.len() {
            rows.push(Vec::new());
        }
        (rows, (first + caret / cols, caret % cols))
    }

    fn submit(&mut self, ctx: &mut Ctx) {
        let line = core::mem::take(&mut self.input);
        self.caret = 0;
        self.history_pos = None;
        // Echo the prompt and command into the scrollback.
        let prompt: String = self.prompt().iter().map(|(c, _)| *c).collect();
        let user_len = "user@mayos".len();
        self.color = GREEN;
        self.print(&prompt[..user_len]);
        self.color = FG;
        self.print(":");
        self.color = BLUE;
        let cwd = self.cwd.clone();
        self.print(&cwd);
        self.color = FG;
        self.print("$ ");
        self.print(&line);
        self.print("\n");
        let trimmed = line.trim();
        if !trimmed.is_empty() {
            if self.history.last().map(|h| h != trimmed).unwrap_or(true) {
                self.history.push(String::from(trimmed));
            }
            super::shell::run(self, trimmed, ctx);
        }
        ctx.redraw();
    }

    /// Start a user program attached to this terminal.
    pub fn start_program(&mut self, path: &str, args: &str) -> Result<(), String> {
        let console = Console::new();
        let p = process::spawn(path, args, &self.cwd, console.clone())?;
        self.process = Some(p);
        self.console = Some(console);
        self.proc_input.clear();
        Ok(())
    }

    /// Scrollback as plain text (used by the self-test).
    pub fn plain_text(&self) -> String {
        let lines: Vec<String> = self.lines.iter().map(|l| l.iter().map(|(c, _)| *c).collect()).collect();
        lines.join("\n")
    }

    pub fn history(&self) -> &[String] {
        &self.history
    }

    fn poll_process(&mut self, ctx: &mut Ctx) {
        let Some(console) = self.console.clone() else { return };
        let out = console.take_output();
        if !out.is_empty() {
            let s = String::from_utf8_lossy(&out).into_owned();
            self.print(&s);
            ctx.redraw();
        }
        let exited = self.process.as_ref().and_then(|p| p.has_exited());
        if let Some(code) = exited {
            let out = console.take_output();
            if !out.is_empty() {
                let s = String::from_utf8_lossy(&out).into_owned();
                self.print(&s);
            }
            if let Some(p) = self.process.take() {
                process::reap(p.pid);
            }
            self.console = None;
            if !self.lines.last().map(|l| l.is_empty()).unwrap_or(true) {
                self.print("\n");
            }
            if code != 0 {
                self.color = DIM;
                self.print(&format!("[exited with code {}]\n", code));
                self.color = FG;
            }
            if self.close_when_done {
                self.color = DIM;
                self.print("[process finished - press any key to close]\n");
                self.color = FG;
            }
            ctx.redraw();
        }
    }

    fn history_step(&mut self, up: bool) {
        if self.history.is_empty() {
            return;
        }
        let n = self.history.len();
        let pos = match (self.history_pos, up) {
            (None, true) => Some(n - 1),
            (None, false) => None,
            (Some(0), true) => Some(0),
            (Some(i), true) => Some(i - 1),
            (Some(i), false) if i + 1 < n => Some(i + 1),
            (Some(_), false) => None,
        };
        self.history_pos = pos;
        self.input = pos.map(|i| self.history[i].clone()).unwrap_or_default();
        self.caret = self.input.chars().count();
    }

    fn byte_at(&self, ch: usize) -> usize {
        self.input.char_indices().nth(ch).map(|(i, _)| i).unwrap_or(self.input.len())
    }

    fn complete(&mut self) {
        let before: String = self.input.chars().take(self.caret).collect();
        let word_start = before.rfind(' ').map(|i| i + 1).unwrap_or(0);
        let word = &before[word_start..];
        let (dir, prefix) = match word.rfind('/') {
            Some(i) => (&word[..=i], &word[i + 1..]),
            None => ("", word),
        };
        let abs = crate::fs::normalize(&self.cwd, if dir.is_empty() { "." } else { dir });
        let Ok(entries) = crate::fs::read_dir(&abs) else { return };
        let lower = prefix.to_lowercase();
        let matches: Vec<_> = entries.iter().filter(|e| e.name.to_lowercase().starts_with(&lower)).collect();
        if matches.len() == 1 {
            let e = matches[0];
            let mut completion = String::from(&e.name[prefix.len().min(e.name.len())..]);
            if e.is_dir {
                completion.push('/');
            } else {
                completion.push(' ');
            }
            // Keep the user's typed prefix but fix up its case.
            let at = self.byte_at(self.caret);
            let fixed_start = word_start + dir.len();
            self.input.replace_range(fixed_start..at, &e.name[..prefix.len().min(e.name.len())]);
            let at = fixed_start + prefix.len().min(e.name.len());
            self.input.insert_str(at, &completion);
            self.caret = self.input[..at + completion.len()].chars().count();
        } else if matches.len() > 1 {
            let names: Vec<String> = matches.iter().map(|e| e.name.clone()).collect();
            let line = format!("{}{}\n", self.prompt().iter().map(|(c, _)| *c).collect::<String>(), self.input);
            self.print(&line);
            self.color = DIM;
            self.print(&names.join("  "));
            self.color = FG;
            self.print("\n");
        }
    }

    fn scrollbar_track(&self) -> Rect {
        Rect::new(self.size.0 - 10, 4, 8, self.size.1 - 8)
    }

    fn scroll_to_y(&mut self, y: i32) {
        let (rows, _) = self.visual_rows();
        let total = rows.len() as i32;
        let view = self.rows;
        if total <= view {
            return;
        }
        let track = self.scrollbar_track();
        let range = total - view;
        let t = ((y - track.y) * range / track.h.max(1)).clamp(0, range);
        self.scroll = range - t;
    }
}

impl App for Terminal {
    fn title(&self) -> String {
        match &self.process {
            Some(p) => format!("{} \u{2014} Terminal", p.name),
            None => format!("Terminal \u{2014} {}", self.cwd),
        }
    }
    fn icon(&self) -> Icon {
        Icon::Terminal
    }
    fn kind(&self) -> AppKind {
        AppKind::Terminal
    }
    fn initial_size(&self) -> (i32, i32) {
        (720, 440)
    }

    fn render(&mut self, c: &mut Canvas, (w, h): (i32, i32), focused: bool) {
        self.size = (w, h);
        self.focused = focused;
        let f = &fonts().mono;
        let cw = self.char_w();
        let lh = self.line_h();
        self.cols = ((w - PAD * 2 - 10) / cw).max(10) as usize;
        self.rows = ((h - PAD * 2) / lh).max(1);
        c.fill_rect(Rect::new(0, 0, w, h), BG);
        let (rows, (crow, ccol)) = self.visual_rows();
        let total = rows.len() as i32;
        let max_scroll = (total - self.rows).max(0);
        self.scroll = self.scroll.clamp(0, max_scroll);
        let first = (total - self.rows - self.scroll).max(0);
        let base0 = PAD + f.ascent;
        for (i, row) in rows.iter().enumerate().skip(first as usize).take(self.rows as usize) {
            let y = base0 + (i as i32 - first) * lh;
            let mut x = PAD;
            let mut run = String::new();
            let mut run_color = FG;
            let mut run_x = x;
            for &(ch, col) in row {
                if col != run_color && !run.is_empty() {
                    c.draw_text(f, run_x, y, &run, run_color);
                    run.clear();
                }
                if run.is_empty() {
                    run_x = x;
                    run_color = col;
                }
                run.push(ch);
                x += cw;
            }
            if !run.is_empty() {
                c.draw_text(f, run_x, y, &run, run_color);
            }
        }
        // Caret.
        let crow = crow as i32;
        if crow >= first && crow < first + self.rows {
            let x = PAD + ccol as i32 * cw;
            let y = PAD + (crow - first) * lh;
            if focused {
                if self.blink_on {
                    c.fill_rect(Rect::new(x, y + 1, cw, lh - 1), gfx::with_alpha(FG, 200));
                    if let Some((ch, _)) = rows[crow as usize].get(ccol) {
                        let mut buf = [0u8; 4];
                        c.draw_text(f, x, y + f.ascent, ch.encode_utf8(&mut buf), BG);
                    }
                }
            } else {
                c.stroke_rounded_rect(Rect::new(x, y + 1, cw, lh - 1), 0, 1, gfx::with_alpha(FG, 160));
            }
        }
        widgets::draw_scrollbar(c, self.scrollbar_track(), total * lh, self.rows * lh, (first) * lh);
    }

    fn event(&mut self, ev: &AppEvent, ctx: &mut Ctx) {
        match ev {
            AppEvent::Key(k) if k.pressed => {
                self.blink_on = true;
                self.last_blink = crate::time::uptime_ms();
                if self.close_when_done && self.process.is_none() {
                    ctx.close();
                    return;
                }
                if self.process.is_some() {
                    match k.key {
                        Key::Char('c') if k.ctrl => {
                            if let Some(p) = &self.process {
                                process::kill(p);
                            }
                        }
                        Key::Char('d') if k.ctrl => {
                            if let Some(con) = &self.console {
                                con.close_input();
                            }
                        }
                        Key::Enter => {
                            let mut line = core::mem::take(&mut self.proc_input);
                            self.print(&line);
                            self.print("\n");
                            line.push('\n');
                            if let Some(con) = &self.console {
                                con.send_input(line.as_bytes());
                            }
                        }
                        Key::Backspace => {
                            self.proc_input.pop();
                        }
                        _ => {
                            if let Some(ch) = k.text() {
                                self.proc_input.push(ch);
                            }
                        }
                    }
                    self.scroll = 0;
                    ctx.redraw();
                    return;
                }
                match k.key {
                    Key::Enter => self.submit(ctx),
                    Key::Backspace => {
                        if self.caret > 0 {
                            let i = self.byte_at(self.caret - 1);
                            self.input.remove(i);
                            self.caret -= 1;
                        }
                    }
                    Key::Delete => {
                        if self.caret < self.input.chars().count() {
                            let i = self.byte_at(self.caret);
                            self.input.remove(i);
                        }
                    }
                    Key::Left => self.caret = self.caret.saturating_sub(1),
                    Key::Right => self.caret = (self.caret + 1).min(self.input.chars().count()),
                    Key::Home => self.caret = 0,
                    Key::End => self.caret = self.input.chars().count(),
                    Key::Up => self.history_step(true),
                    Key::Down => self.history_step(false),
                    Key::PageUp => self.scroll += self.rows / 2,
                    Key::PageDown => self.scroll = (self.scroll - self.rows / 2).max(0),
                    Key::Tab => self.complete(),
                    Key::Char('l') if k.ctrl => self.clear(),
                    Key::Char('c') if k.ctrl => {
                        self.input.clear();
                        self.caret = 0;
                        self.print("^C\n");
                    }
                    Key::Char('u') if k.ctrl => {
                        let at = self.byte_at(self.caret);
                        self.input.replace_range(..at, "");
                        self.caret = 0;
                    }
                    _ => {
                        if let Some(ch) = k.text() {
                            let i = self.byte_at(self.caret);
                            self.input.insert(i, ch);
                            self.caret += 1;
                        }
                    }
                }
                if !matches!(k.key, Key::PageUp | Key::PageDown) {
                    self.scroll = 0;
                }
                ctx.redraw();
            }
            AppEvent::Wheel { delta, .. } => {
                self.scroll = (self.scroll - delta * 3).max(0);
                ctx.redraw();
            }
            AppEvent::MouseDown { x, y, button: 0, .. } => {
                if self.scrollbar_track().inset(-4).contains(*x, *y) {
                    self.dragging_scrollbar = true;
                    self.scroll_to_y(*y);
                    ctx.redraw();
                }
            }
            AppEvent::MouseMove { y, .. } if self.dragging_scrollbar => {
                self.scroll_to_y(*y);
                ctx.redraw();
            }
            AppEvent::MouseUp { .. } => self.dragging_scrollbar = false,
            AppEvent::Focus(_) | AppEvent::Resized { .. } => ctx.redraw(),
            _ => {}
        }
    }

    fn tick(&mut self, ctx: &mut Ctx) {
        if let Some(cmd) = self.startup.take() {
            super::shell::run(self, &cmd, ctx);
            if self.process.is_some() {
                self.close_when_done = true;
            }
            ctx.redraw();
        }
        self.poll_process(ctx);
        let now = crate::time::uptime_ms();
        if self.focused && now - self.last_blink >= 530 {
            self.last_blink = now;
            self.blink_on = !self.blink_on;
            ctx.redraw();
        }
    }

    fn request_close(&mut self, _ctx: &mut Ctx) -> bool {
        if let Some(p) = self.process.take() {
            process::kill(&p);
            process::reap(p.pid);
        }
        true
    }
}
