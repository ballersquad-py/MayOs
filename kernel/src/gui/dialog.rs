//! Small modal-style dialogs that report back to the window that opened
//! them with `Msg::DialogResult`.

use alloc::string::String;

use gfx::{Canvas, Rect};

use super::app::{App, AppEvent, Ctx, Msg, WindowId};
use super::theme::{self, fonts};
use super::widgets::{button, ButtonStyle, TextInput};
use crate::input::Key;

pub struct Dialog {
    title: String,
    message: String,
    input: Option<TextInput>,
    ok_label: String,
    cancel_label: Option<String>,
    danger: bool,
    /// The second button is an alternative answer rather than "Cancel".
    second_is_alt: bool,
    tag: u32,
    reply_to: WindowId,
    hover: Option<u8>,
    size: (i32, i32),
    answered: bool,
}

impl Dialog {
    /// Ask for a line of text (e.g. a file name).
    pub fn input(title: &str, message: &str, initial: &str, ok: &str, tag: u32, reply_to: WindowId) -> Dialog {
        let mut input = TextInput::new(initial);
        input.select_stem();
        Dialog {
            title: title.into(),
            message: message.into(),
            input: Some(input),
            ok_label: ok.into(),
            cancel_label: Some("Cancel".into()),
            danger: false,
            second_is_alt: false,
            tag,
            reply_to,
            hover: None,
            size: (400, 160),
            answered: false,
        }
    }

    /// Ask for confirmation.
    pub fn confirm(title: &str, message: &str, ok: &str, danger: bool, tag: u32, reply_to: WindowId) -> Dialog {
        Dialog {
            title: title.into(),
            message: message.into(),
            input: None,
            ok_label: ok.into(),
            cancel_label: Some("Cancel".into()),
            danger,
            second_is_alt: false,
            tag,
            reply_to,
            hover: None,
            size: (400, 140),
            answered: false,
        }
    }

    /// Three-way choice: OK (value "ok"), alternative (value "alt"), cancel.
    pub fn choice(title: &str, message: &str, ok: &str, alt: &str, tag: u32, reply_to: WindowId) -> Dialog {
        let mut d = Dialog::confirm(title, message, ok, false, tag, reply_to);
        d.cancel_label = Some(alt.into());
        d.second_is_alt = true;
        d.size = (420, 140);
        d
    }

    fn ok_rect(&self) -> Rect {
        let (w, h) = self.size;
        Rect::new(w - 110, h - 44, 94, 30)
    }

    fn cancel_rect(&self) -> Rect {
        let (w, h) = self.size;
        Rect::new(w - 214, h - 44, 94, 30)
    }

    fn finish(&mut self, answer: Answer, ctx: &mut Ctx) {
        if self.answered {
            return;
        }
        self.answered = true;
        let value = match answer {
            Answer::Ok => Some(self.input.as_ref().map(|i| i.text.clone()).unwrap_or_else(|| String::from("ok"))),
            Answer::Second if self.second_is_alt => Some(String::from("alt")),
            _ => None,
        };
        ctx.send(self.reply_to, Msg::DialogResult { tag: self.tag, value });
        ctx.close();
    }
}

#[derive(Clone, Copy)]
enum Answer {
    Ok,
    Second,
    Cancel,
}

impl App for Dialog {
    fn title(&self) -> String {
        self.title.clone()
    }
    fn initial_size(&self) -> (i32, i32) {
        self.size
    }
    fn resizable(&self) -> bool {
        false
    }

    fn render(&mut self, c: &mut Canvas, (w, h): (i32, i32), focused: bool) {
        self.size = (w, h);
        let f = fonts();
        c.fill_rect(Rect::new(0, 0, w, h), theme::PANEL_BG);
        c.draw_text_clipped(&f.ui, 18, 28, &self.message, w - 36, theme::TEXT);
        if let Some(input) = self.input.as_mut() {
            input.render(c, Rect::new(16, 44, w - 32, 32), focused);
        }
        let style = if self.danger { ButtonStyle::Danger } else { ButtonStyle::Primary };
        button(c, self.ok_rect(), &self.ok_label, style, self.hover == Some(0), true);
        if let Some(cancel) = &self.cancel_label {
            button(c, self.cancel_rect(), cancel, ButtonStyle::Normal, self.hover == Some(1), true);
        }
    }

    fn event(&mut self, ev: &AppEvent, ctx: &mut Ctx) {
        match ev {
            AppEvent::MouseMove { x, y, .. } => {
                let h = if self.ok_rect().contains(*x, *y) {
                    Some(0)
                } else if self.cancel_label.is_some() && self.cancel_rect().contains(*x, *y) {
                    Some(1)
                } else {
                    None
                };
                if h != self.hover {
                    self.hover = h;
                    ctx.redraw();
                }
            }
            AppEvent::MouseDown { x, y, .. } => {
                if self.ok_rect().contains(*x, *y) {
                    self.finish(Answer::Ok, ctx);
                } else if self.cancel_label.is_some() && self.cancel_rect().contains(*x, *y) {
                    self.finish(Answer::Second, ctx);
                }
            }
            AppEvent::Key(k) if k.pressed => match k.key {
                Key::Enter => self.finish(Answer::Ok, ctx),
                Key::Escape => self.finish(Answer::Cancel, ctx),
                _ => {
                    if let Some(i) = self.input.as_mut()
                        && i.key(k)
                    {
                        ctx.redraw();
                    }
                }
            },
            _ => {}
        }
    }

    fn request_close(&mut self, ctx: &mut Ctx) -> bool {
        if !self.answered {
            self.answered = true;
            ctx.send(self.reply_to, Msg::DialogResult { tag: self.tag, value: None });
        }
        true
    }
}
