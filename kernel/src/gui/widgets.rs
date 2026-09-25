//! Small reusable UI pieces: buttons, a single-line text field, scrollbars.

use alloc::string::String;

use gfx::{rgb, with_alpha, Canvas, Font, Rect};

use super::theme::{self, fonts};
use crate::input::{Key, KeyEvent};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ButtonStyle {
    Normal,
    Primary,
    Danger,
    Flat,
}

pub fn button(c: &mut Canvas, r: Rect, label: &str, style: ButtonStyle, hovered: bool, enabled: bool) {
    let f = &fonts().ui;
    let (bg, fg, border) = match style {
        ButtonStyle::Primary => (if hovered { theme::accent_dark() } else { theme::accent() }, theme::TEXT_ON_ACCENT, 0),
        ButtonStyle::Danger => (if hovered { rgb(0xc9, 0x35, 0x3a) } else { theme::DANGER }, theme::TEXT_ON_ACCENT, 0),
        ButtonStyle::Normal => (
            if hovered { rgb(0xf0, 0xf2, 0xf5) } else { rgb(0xff, 0xff, 0xff) },
            theme::TEXT,
            with_alpha(0x000000, 45),
        ),
        ButtonStyle::Flat => (if hovered { with_alpha(0x000000, 18) } else { 0 }, theme::TEXT, 0),
    };
    if bg != 0 {
        c.fill_rounded_rect(r, 7, bg);
    }
    if border != 0 {
        c.stroke_rounded_rect(r, 7, 1, border);
    }
    let fg = if enabled { fg } else { with_alpha(fg, 110) };
    c.draw_text_centered(f, r, label, fg);
}

/// A single-line editable text field.
pub struct TextInput {
    pub text: String,
    /// Caret position in characters.
    pub caret: usize,
    scroll: i32,
    /// Select-all state: typing replaces everything.
    pub all_selected: bool,
}

impl TextInput {
    pub fn new(text: &str) -> TextInput {
        let n = text.chars().count();
        TextInput { text: String::from(text), caret: n, scroll: 0, all_selected: false }
    }

    fn byte_index(&self, ch: usize) -> usize {
        self.text.char_indices().nth(ch).map(|(i, _)| i).unwrap_or(self.text.len())
    }

    /// Select the file stem (text before the last dot) like most file
    /// managers do when renaming.
    pub fn select_stem(&mut self) {
        self.all_selected = true;
        let stem = match self.text.rfind('.') {
            Some(i) if i > 0 => self.text[..i].chars().count(),
            _ => self.text.chars().count(),
        };
        self.caret = stem;
    }

    /// Returns true if the key was consumed.
    pub fn key(&mut self, k: &KeyEvent) -> bool {
        if !k.pressed {
            return false;
        }
        let len = self.text.chars().count();
        if let Some(ch) = k.text()
            && ch != '\n'
            && ch != '\t'
            && ch != '\x08'
            && ch != '\x1b'
        {
            if self.all_selected {
                // Replace the selection (everything before the caret).
                let end = self.byte_index(self.caret);
                self.text.replace_range(..end, "");
                self.caret = 0;
                self.all_selected = false;
            }
            let i = self.byte_index(self.caret);
            self.text.insert(i, ch);
            self.caret += 1;
            return true;
        }
        let was_selected = core::mem::replace(&mut self.all_selected, false);
        match k.key {
            Key::Backspace => {
                if was_selected {
                    let end = self.byte_index(self.caret);
                    self.text.replace_range(..end, "");
                    self.caret = 0;
                } else if self.caret > 0 {
                    let i = self.byte_index(self.caret - 1);
                    self.text.remove(i);
                    self.caret -= 1;
                }
            }
            Key::Delete => {
                if self.caret < len {
                    let i = self.byte_index(self.caret);
                    self.text.remove(i);
                }
            }
            Key::Left => self.caret = self.caret.saturating_sub(1),
            Key::Right => self.caret = (self.caret + 1).min(len),
            Key::Home => self.caret = 0,
            Key::End => self.caret = len,
            Key::Char('a') if k.ctrl => {
                self.caret = len;
                self.all_selected = true;
            }
            _ => return false,
        }
        true
    }

    pub fn render(&mut self, c: &mut Canvas, r: Rect, focused: bool) {
        let f: &Font = &fonts().ui;
        c.fill_rounded_rect(r, 6, rgb(0xff, 0xff, 0xff));
        let border = if focused { theme::accent() } else { with_alpha(0x000000, 50) };
        c.stroke_rounded_rect(r, 6, if focused { 2 } else { 1 }, border);
        let inner = r.inset(8);
        let carets = f.caret_positions(&self.text);
        let caret_x = carets[self.caret.min(carets.len() - 1)];
        if caret_x - self.scroll > inner.w - 2 {
            self.scroll = caret_x - inner.w + 2;
        }
        if caret_x < self.scroll {
            self.scroll = caret_x;
        }
        let old = c.push_clip(Rect::new(inner.x, r.y, inner.w, r.h));
        let base = r.y + (r.h + f.ascent - f.descent) / 2;
        if self.all_selected && focused {
            let w = caret_x.max(4);
            c.fill_rect(Rect::new(inner.x - self.scroll, r.y + 5, w, r.h - 10), theme::selection());
        }
        c.draw_text(f, inner.x - self.scroll, base, &self.text, theme::TEXT);
        if focused && !self.all_selected {
            c.fill_rect(Rect::new(inner.x - self.scroll + caret_x, r.y + 6, 1, r.h - 12), theme::accent());
        }
        c.restore_clip(old);
    }
}

/// Geometry of a vertical scrollbar thumb for `content` px shown through a
/// `view` px viewport at `offset`.
pub fn scroll_thumb(track: Rect, content: i32, view: i32, offset: i32) -> Option<Rect> {
    if content <= view || track.h <= 0 {
        return None;
    }
    let h = (track.h * view / content).max(24).min(track.h);
    let range = content - view;
    let y = track.y + (track.h - h) * offset.clamp(0, range) / range.max(1);
    Some(Rect::new(track.x, y, track.w, h))
}

pub fn draw_scrollbar(c: &mut Canvas, track: Rect, content: i32, view: i32, offset: i32) {
    if let Some(t) = scroll_thumb(track, content, view, offset) {
        c.fill_rounded_rect(t.inset(2), 3, with_alpha(0x000000, 70));
    }
}
