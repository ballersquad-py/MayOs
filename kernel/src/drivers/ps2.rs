//! PS/2 keyboard (scancode set 1 via controller translation) and mouse.

use crate::arch::cpu::{inb, outb};
use crate::input::{self, InputEvent, Key, KeyEvent};
use crate::sync::Spin;

const DATA: u16 = 0x60;
const STATUS: u16 = 0x64;
const CMD: u16 = 0x64;

fn wait_write() {
    for _ in 0..100_000 {
        if unsafe { inb(STATUS) } & 2 == 0 {
            return;
        }
    }
}

fn wait_read() -> bool {
    for _ in 0..100_000 {
        if unsafe { inb(STATUS) } & 1 != 0 {
            return true;
        }
    }
    false
}

fn command(c: u8) {
    wait_write();
    unsafe { outb(CMD, c) };
}

fn write_data(v: u8) {
    wait_write();
    unsafe { outb(DATA, v) };
}

fn read_data() -> Option<u8> {
    if wait_read() { Some(unsafe { inb(DATA) }) } else { None }
}

fn mouse_write(v: u8) -> Option<u8> {
    command(0xd4);
    write_data(v);
    read_data()
}

struct State {
    extended: bool,
    shift_l: bool,
    shift_r: bool,
    ctrl: bool,
    alt: bool,
    caps: bool,
    packet: [u8; 4],
    packet_len: usize,
    packet_size: usize,
}

static STATE: Spin<State> = Spin::new(State {
    extended: false,
    shift_l: false,
    shift_r: false,
    ctrl: false,
    alt: false,
    caps: false,
    packet: [0; 4],
    packet_len: 0,
    packet_size: 3,
});

/// Initialise the controller. Returns whether a wheel mouse was detected.
pub fn init() -> bool {
    command(0xad);
    command(0xa7);
    while unsafe { inb(STATUS) } & 1 != 0 {
        unsafe { inb(DATA) };
    }
    command(0x20);
    let mut cfg = read_data().unwrap_or(0);
    cfg |= 0x03; // IRQ1 + IRQ12
    cfg &= !0x30; // clocks enabled for both ports
    command(0x60);
    write_data(cfg);
    command(0xae);
    command(0xa8);

    // Keyboard: enable scanning.
    write_data(0xf4);
    let _ = read_data();

    // Mouse: defaults, try the IntelliMouse wheel sequence, then enable.
    let _ = mouse_write(0xf6);
    for rate in [200u8, 100, 80] {
        let _ = mouse_write(0xf3);
        let _ = mouse_write(rate);
    }
    let _ = mouse_write(0xf2);
    let id = read_data().unwrap_or(0);
    let wheel = id == 3;
    let _ = mouse_write(0xf4);
    STATE.lock().packet_size = if wheel { 4 } else { 3 };
    wheel
}

fn set1_key(code: u8, extended: bool, shift: bool, caps: bool) -> Key {
    if extended {
        return match code {
            0x1c => Key::Enter,
            0x1d => Key::Ctrl,
            0x35 => Key::Char('/'),
            0x38 => Key::Alt,
            0x47 => Key::Home,
            0x48 => Key::Up,
            0x49 => Key::PageUp,
            0x4b => Key::Left,
            0x4d => Key::Right,
            0x4f => Key::End,
            0x50 => Key::Down,
            0x51 => Key::PageDown,
            0x52 => Key::Insert,
            0x53 => Key::Delete,
            0x5b | 0x5c => Key::Super,
            _ => Key::Unknown,
        };
    }
    const LOWER: &[u8; 58] = b"\x00\x1b1234567890-=\x08\tqwertyuiop[]\n\x00asdfghjkl;'`\x00\\zxcvbnm,./\x00*\x00 ";
    const UPPER: &[u8; 58] = b"\x00\x1b!@#$%^&*()_+\x08\tQWERTYUIOP{}\n\x00ASDFGHJKL:\"~\x00|ZXCVBNM<>?\x00*\x00 ";
    match code {
        0x01 => Key::Escape,
        0x0e => Key::Backspace,
        0x0f => Key::Tab,
        0x1c => Key::Enter,
        0x1d => Key::Ctrl,
        0x2a | 0x36 => Key::Shift,
        0x38 => Key::Alt,
        0x3a => Key::CapsLock,
        0x3b..=0x44 => Key::F(code - 0x3b + 1),
        0x57 => Key::F(11),
        0x58 => Key::F(12),
        0x47 => Key::Char('7'),
        0x48 => Key::Char('8'),
        0x49 => Key::Char('9'),
        0x4a => Key::Char('-'),
        0x4b => Key::Char('4'),
        0x4c => Key::Char('5'),
        0x4d => Key::Char('6'),
        0x4e => Key::Char('+'),
        0x4f => Key::Char('1'),
        0x50 => Key::Char('2'),
        0x51 => Key::Char('3'),
        0x52 => Key::Char('0'),
        0x53 => Key::Char('.'),
        c if (c as usize) < LOWER.len() => {
            let lower = LOWER[c as usize];
            if lower == 0 {
                return Key::Unknown;
            }
            let letter = lower.is_ascii_lowercase();
            let upper = if letter { shift ^ caps } else { shift };
            Key::Char(if upper { UPPER[c as usize] } else { lower } as char)
        }
        _ => Key::Unknown,
    }
}

pub fn on_keyboard_irq() {
    let code = unsafe { inb(DATA) };
    let mut s = STATE.lock();
    if code == 0xe0 {
        s.extended = true;
        return;
    }
    let extended = core::mem::replace(&mut s.extended, false);
    let pressed = code & 0x80 == 0;
    let make = code & 0x7f;
    // Ignore fake shifts that some keyboards wrap around extended keys.
    if extended && (make == 0x2a || make == 0x36) {
        return;
    }
    let key = set1_key(make, extended, s.shift_l || s.shift_r, s.caps);
    match key {
        Key::Shift => {
            if make == 0x2a {
                s.shift_l = pressed;
            } else {
                s.shift_r = pressed;
            }
        }
        Key::Ctrl => s.ctrl = pressed,
        Key::Alt => s.alt = pressed,
        Key::CapsLock if pressed => s.caps = !s.caps,
        _ => {}
    }
    let ev = KeyEvent { key, pressed, shift: s.shift_l || s.shift_r, ctrl: s.ctrl, alt: s.alt };
    drop(s);
    input::push(InputEvent::Key(ev));
}

pub fn on_mouse_irq() {
    let b = unsafe { inb(DATA) };
    let mut s = STATE.lock();
    if s.packet_len == 0 && b & 0x08 == 0 {
        return; // out of sync: wait for a valid first byte
    }
    let i = s.packet_len;
    s.packet[i] = b;
    s.packet_len += 1;
    if s.packet_len < s.packet_size {
        return;
    }
    s.packet_len = 0;
    let p = s.packet;
    let wheel_size = s.packet_size;
    drop(s);

    let dx = p[1] as i32 - (((p[0] as i32) << 4) & 0x100);
    let dy = p[2] as i32 - (((p[0] as i32) << 3) & 0x100);
    if dx != 0 || dy != 0 {
        input::push(InputEvent::MouseMove { dx, dy: -dy });
    }
    static BUTTONS: Spin<u8> = Spin::new(0);
    let mut prev = BUTTONS.lock();
    let now = p[0] & 0x7;
    for (bit, button) in [(1u8, 0u8), (2, 1), (4, 2)] {
        if (now ^ *prev) & bit != 0 {
            input::push(InputEvent::MouseButton { button, pressed: now & bit != 0 });
        }
    }
    *prev = now;
    if wheel_size == 4 {
        let z = (p[3] & 0x0f) as i32;
        let z = if z & 0x08 != 0 { z - 16 } else { z };
        if z != 0 {
            input::push(InputEvent::Wheel(z));
        }
    }
}
