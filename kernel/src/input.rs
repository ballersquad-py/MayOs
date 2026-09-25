//! Input events from all devices, queued for the window manager.

use alloc::collections::VecDeque;

use crate::sync::Spin;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    Char(char),
    Enter,
    Backspace,
    Tab,
    Escape,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    Delete,
    Insert,
    F(u8),
    Shift,
    Ctrl,
    Alt,
    Super,
    CapsLock,
    Unknown,
}

#[derive(Clone, Copy, Debug)]
pub struct KeyEvent {
    pub key: Key,
    pub pressed: bool,
    pub shift: bool,
    pub ctrl: bool,
    pub alt: bool,
}

impl KeyEvent {
    /// Printable character produced by this key press, if any.
    pub fn text(&self) -> Option<char> {
        match self.key {
            Key::Char(c) if !self.ctrl && !self.alt => Some(c),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub enum InputEvent {
    Key(KeyEvent),
    /// Relative motion (PS/2 mouse). Positive dy is down.
    MouseMove { dx: i32, dy: i32 },
    /// Absolute position scaled to 0..=65535 (tablet).
    MouseAbsolute { x: Option<u32>, y: Option<u32> },
    MouseButton { button: u8, pressed: bool },
    /// Positive scrolls down.
    Wheel(i32),
}

static QUEUE: Spin<VecDeque<InputEvent>> = Spin::new(VecDeque::new());

pub fn push(e: InputEvent) {
    let mut q = QUEUE.lock();
    if q.len() < 1024 {
        q.push_back(e);
    }
}

pub fn pop() -> Option<InputEvent> {
    QUEUE.lock().pop_front()
}
