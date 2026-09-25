//! The interface between the window manager and applications.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;

use gfx::icons::Icon;
use gfx::{Canvas, Rect};

use crate::input::KeyEvent;

pub type WindowId = u32;

#[derive(Clone, Debug)]
pub enum Msg {
    /// Result of a dialog: `None` when cancelled.
    DialogResult { tag: u32, value: Option<String> },
    /// Reply to `Command::SetResolution`.
    ResolutionResult(bool),
}

/// Events delivered to apps; not every app reads every field.
#[allow(dead_code)]
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
    SetResolution(u32, u32),
    /// Go back to a previous mode without asking again.
    RevertResolution(u32, u32),
    Shutdown,
    Reboot,
    /// Cover the whole screen without decorations (or go back).
    SetFullscreen(bool),
}

/// Per-call context handed to apps.
pub struct Ctx {
    pub window: WindowId,
    pub redraw: bool,
    /// Part of the client area to redraw when `redraw` is not set.
    pub redraw_area: Option<Rect>,
    pub commands: Vec<Command>,
}

impl Ctx {
    pub fn new(window: WindowId) -> Ctx {
        Ctx { window, redraw: false, redraw_area: None, commands: Vec::new() }
    }

    pub fn redraw(&mut self) {
        self.redraw = true;
    }

    /// Redraw only `r` (client coordinates). The app's `render` is called
    /// with a clip, so it may draw everything and only `r` changes.
    pub fn redraw_rect(&mut self, r: Rect) {
        self.redraw_area = Some(match self.redraw_area {
            Some(a) => a.union(&r),
            None => r,
        });
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

    pub fn set_fullscreen(&mut self, on: bool) {
        self.commands.push(Command::SetFullscreen(on));
    }

    pub fn send(&mut self, to: WindowId, msg: Msg) {
        self.commands.push(Command::Send(to, msg));
    }
}

pub trait App {
    fn title(&self) -> String;

    /// Icon for the app (used by future task switchers).
    #[allow(dead_code)]
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
    Settings,
    About,
    Browser,
    Other,
}
