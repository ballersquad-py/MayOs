//! The interface between the window manager and applications.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;

use gfx::icons::Icon;
use gfx::Canvas;

use crate::input::KeyEvent;

pub type WindowId = u32;

#[derive(Clone, Debug)]
pub enum Msg {
    /// Result of a dialog: `None` when cancelled.
    DialogResult { tag: u32, value: Option<String> },
}

#[derive(Clone, Debug)]
pub enum AppEvent {
    MouseDown { x: i32, y: i32, button: u8, clicks: u8 },
    MouseUp { x: i32, y: i32, button: u8 },
    MouseMove { x: i32, y: i32, buttons: u8 },
    MouseLeave,
    Wheel { x: i32, y: i32, delta: i32 },
    Key(KeyEvent),
    Resized { w: i32, h: i32 },
    Focus(bool),
    Message(Msg),
}

pub enum Command {
    Open(Box<dyn App>),
    /// Open a window centred over the sender.
    OpenChild(Box<dyn App>),
    Close,
    Send(WindowId, Msg),
    Shutdown,
    Reboot,
}

/// Per-call context handed to apps.
pub struct Ctx {
    pub window: WindowId,
    pub redraw: bool,
    pub commands: Vec<Command>,
}

impl Ctx {
    pub fn new(window: WindowId) -> Ctx {
        Ctx { window, redraw: false, commands: Vec::new() }
    }

    pub fn redraw(&mut self) {
        self.redraw = true;
    }

    pub fn open(&mut self, app: Box<dyn App>) {
        self.commands.push(Command::Open(app));
    }

    pub fn open_child(&mut self, app: Box<dyn App>) {
        self.commands.push(Command::OpenChild(app));
    }

    pub fn close(&mut self) {
        self.commands.push(Command::Close);
    }

    pub fn send(&mut self, to: WindowId, msg: Msg) {
        self.commands.push(Command::Send(to, msg));
    }
}

pub trait App {
    fn title(&self) -> String;

    fn icon(&self) -> Icon {
        Icon::File
    }

    /// Which dock entry this window belongs to.
    fn kind(&self) -> AppKind {
        AppKind::Other
    }

    fn initial_size(&self) -> (i32, i32) {
        (640, 440)
    }

    fn min_size(&self) -> (i32, i32) {
        (260, 160)
    }

    fn resizable(&self) -> bool {
        true
    }

    /// Draw the client area (origin at its top-left, `size` is its size).
    fn render(&mut self, c: &mut Canvas, size: (i32, i32), focused: bool);

    fn event(&mut self, ev: &AppEvent, ctx: &mut Ctx);

    /// Called about 60 times a second.
    fn tick(&mut self, _ctx: &mut Ctx) {}

    /// Return false to veto closing (e.g. to ask about unsaved changes).
    fn request_close(&mut self, _ctx: &mut Ctx) -> bool {
        true
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AppKind {
    Explorer,
    Terminal,
    Editor,
    About,
    Other,
}
