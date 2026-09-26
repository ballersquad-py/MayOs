//! Graphics for Linux programs: a Linux framebuffer (`/dev/fb0`) and
//! evdev input devices (`/dev/input/event0` keyboard, `event1` pointer,
//! `/dev/input/mice`) backed by a MayOS window.
//!
//! The pixels live in physically contiguous memory that the program maps
//! with `mmap` and the desktop reads directly, so drawing costs no copies
//! and no system calls. Each Linux process gets one screen, created the
//! first time it opens any of these devices.

use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::input::{Key, KeyEvent};
use crate::mem::{phys_to_virt, pmm, PAGE_SIZE};
use crate::sync::Spin;

pub const DEFAULT_SIZE: (u32, u32) = (800, 600);
const MAX_SIZE: (u32, u32) = (1920, 1200);
const QUEUE_MAX: usize = 1024;

pub struct Buffer {
    pub w: u32,
    pub h: u32,
    pub phys: u64,
    pub pages: usize,
}

impl Buffer {
    fn new(w: u32, h: u32) -> Option<Buffer> {
        let pages = ((w * h * 4) as u64).div_ceil(PAGE_SIZE) as usize;
        let phys = pmm::alloc_contiguous(pages)?;
        unsafe { core::ptr::write_bytes(phys_to_virt(phys) as *mut u8, 0, pages * PAGE_SIZE as usize) };
        Some(Buffer { w, h, phys, pages })
    }

    /// The pixels (0x00RRGGBB, `w` per row).
    pub fn pixels(&self) -> &[u32] {
        unsafe { core::slice::from_raw_parts(phys_to_virt(self.phys) as *const u32, (self.w * self.h) as usize) }
    }
}

impl Drop for Buffer {
    fn drop(&mut self) {
        pmm::free_contiguous(self.phys, self.pages);
    }
}

pub struct Screen {
    pub pid: u64,
    pub title: String,
    /// Current buffer. A program may still change the mode before it maps
    /// the memory; afterwards the buffer stays put.
    pub buf: Spin<Arc<Buffer>>,
    /// Buffers the program has mapped: freed only with the screen.
    mapped: Spin<Vec<Arc<Buffer>>>,
    pub keys: Spin<VecDeque<[u8; 24]>>,
    pub pointer: Spin<VecDeque<[u8; 24]>>,
    pub mice: Spin<VecDeque<u8>>,
    /// The window was closed by the user.
    pub closed: AtomicBool,
    /// Bumped on FBIOPAN_DISPLAY / vsync waits so the window knows the
    /// program presents frames explicitly.
    pub presents: AtomicU64,
    pub last_pos: Spin<(i32, i32)>,
    pub buttons: Spin<u8>,
}

static PENDING: Spin<Vec<Arc<Screen>>> = Spin::new(Vec::new());

/// Screens waiting for the desktop to give them a window.
pub fn take_pending() -> Vec<Arc<Screen>> {
    core::mem::take(&mut *PENDING.lock())
}

impl Screen {
    pub fn new(pid: u64, title: String) -> Option<Arc<Screen>> {
        let (w, h) = DEFAULT_SIZE;
        let s = Arc::new(Screen {
            pid,
            title,
            buf: Spin::new(Arc::new(Buffer::new(w, h)?)),
            mapped: Spin::new(Vec::new()),
            keys: Spin::new(VecDeque::new()),
            pointer: Spin::new(VecDeque::new()),
            mice: Spin::new(VecDeque::new()),
            closed: AtomicBool::new(false),
            presents: AtomicU64::new(0),
            last_pos: Spin::new((w as i32 / 2, h as i32 / 2)),
            buttons: Spin::new(0),
        });
        PENDING.lock().push(s.clone());
        Some(s)
    }

    pub fn size(&self) -> (u32, u32) {
        let b = self.buf.lock();
        (b.w, b.h)
    }

    /// Change the mode (FBIOPUT_VSCREENINFO). Fails once the memory is
    /// mapped at another size.
    pub fn set_size(&self, w: u32, h: u32) -> bool {
        if w == 0 || h == 0 || w > MAX_SIZE.0 || h > MAX_SIZE.1 {
            return false;
        }
        let mut b = self.buf.lock();
        if (b.w, b.h) == (w, h) {
            return true;
        }
        if !self.mapped.lock().is_empty() {
            return false;
        }
        match Buffer::new(w, h) {
            Some(n) => {
                *b = Arc::new(n);
                true
            }
            None => false,
        }
    }

    /// Physical address and page count of the buffer, pinned for mapping.
    pub fn map(&self) -> (u64, usize) {
        let b = self.buf.lock().clone();
        let r = (b.phys, b.pages);
        let mut m = self.mapped.lock();
        if !m.iter().any(|x| Arc::ptr_eq(x, &b)) {
            m.push(b);
        }
        r
    }

    fn push(q: &Spin<VecDeque<[u8; 24]>>, events: &[(u16, u16, i32)]) {
        super::sched::notify();
        let us = super::linux::unix_ms() * 1000 + crate::time::uptime_us() % 1000;
        let mut q = q.lock();
        if q.len() + events.len() + 1 > QUEUE_MAX {
            return;
        }
        for &(ty, code, value) in events.iter().chain(core::iter::once(&(0u16, 0u16, 0i32))) {
            let mut e = [0u8; 24];
            e[0..8].copy_from_slice(&((us / 1_000_000) as i64).to_le_bytes());
            e[8..16].copy_from_slice(&((us % 1_000_000) as i64).to_le_bytes());
            e[16..18].copy_from_slice(&ty.to_le_bytes());
            e[18..20].copy_from_slice(&code.to_le_bytes());
            e[20..24].copy_from_slice(&value.to_le_bytes());
            q.push_back(e);
        }
    }

    /// A key from the window.
    pub fn key(&self, k: &KeyEvent) {
        let Some(code) = keycode(k.key) else { return };
        Self::push(&self.keys, &[(EV_KEY, code, k.pressed as i32)]);
    }

    /// Pointer moved to `(x, y)` in framebuffer pixels.
    pub fn motion(&self, x: i32, y: i32) {
        let (w, h) = self.size();
        let (x, y) = (x.clamp(0, w as i32 - 1), y.clamp(0, h as i32 - 1));
        let mut last = self.last_pos.lock();
        let (dx, dy) = (x - last.0, y - last.1);
        if dx == 0 && dy == 0 {
            return;
        }
        *last = (x, y);
        drop(last);
        Self::push(&self.pointer, &[(EV_ABS, 0, x), (EV_ABS, 1, y), (EV_REL, 0, dx), (EV_REL, 1, dy)]);
        self.mice_packet(dx, dy);
    }

    /// Button 0 = left, 1 = right, 2 = middle.
    pub fn button(&self, button: u8, down: bool) {
        let bit = 1u8 << button.min(2);
        {
            let mut b = self.buttons.lock();
            if down { *b |= bit } else { *b &= !bit }
        }
        Self::push(&self.pointer, &[(EV_KEY, 0x110 + button.min(2) as u16, down as i32)]);
        self.mice_packet(0, 0);
    }

    pub fn wheel(&self, delta: i32) {
        Self::push(&self.pointer, &[(EV_REL, 8, -delta.signum())]);
    }

    /// PS/2 packet for /dev/input/mice.
    fn mice_packet(&self, dx: i32, dy: i32) {
        let (dx, dy) = (dx.clamp(-255, 255), (-dy).clamp(-255, 255));
        let b = *self.buttons.lock();
        let mut h = 0x08 | (b & 1) | ((b & 2) << 0) | ((b & 4) >> 0);
        if dx < 0 {
            h |= 0x10;
        }
        if dy < 0 {
            h |= 0x20;
        }
        let mut q = self.mice.lock();
        if q.len() < QUEUE_MAX * 3 {
            q.extend([h, dx as u8, dy as u8]);
        }
    }
}

pub const EV_KEY: u16 = 1;
pub const EV_REL: u16 = 2;
pub const EV_ABS: u16 = 3;

/// Linux key code of a MayOS key (US layout).
pub fn keycode(k: Key) -> Option<u16> {
    Some(match k {
        Key::Escape => 1,
        Key::Backspace => 14,
        Key::Tab => 15,
        Key::Enter => 28,
        Key::Ctrl => 29,
        Key::Shift => 42,
        Key::Alt => 56,
        Key::CapsLock => 58,
        Key::Super => 125,
        Key::F(n @ 1..=10) => 58 + n as u16,
        Key::F(11) => 87,
        Key::F(12) => 88,
        Key::Home => 102,
        Key::Up => 103,
        Key::PageUp => 104,
        Key::Left => 105,
        Key::Right => 106,
        Key::End => 107,
        Key::Down => 108,
        Key::PageDown => 109,
        Key::Insert => 110,
        Key::Delete => 111,
        Key::Char(c) => {
            let c = c.to_ascii_lowercase();
            const ROWS: [(&str, &str, u16); 4] = [
                ("1234567890-=", "!@#$%^&*()_+", 2),
                ("qwertyuiop[]", "QWERTYUIOP{}", 16),
                ("asdfghjkl;'`", "ASDFGHJKL:\"~", 30),
                ("\\zxcvbnm,./", "|ZXCVBNM<>?", 43),
            ];
            if c == ' ' {
                return Some(57);
            }
            ROWS.iter().find_map(|(lo, hi, base)| {
                lo.chars().position(|x| x == c).or_else(|| hi.chars().position(|x| x == c)).map(|i| base + i as u16)
            })?
        }
        _ => return None,
    })
}

/// Which input a descriptor reads.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum InputKind {
    Keyboard,
    Pointer,
    Mice,
}

impl InputKind {
    pub fn name(self) -> &'static str {
        match self {
            InputKind::Keyboard => "MayOS Keyboard",
            InputKind::Pointer => "MayOS Pointer",
            InputKind::Mice => "MayOS Mice",
        }
    }
}

/// Bytes available to read from an input device.
pub fn has_input(s: &Screen, k: InputKind) -> bool {
    s.closed.load(Ordering::Relaxed)
        || match k {
            InputKind::Keyboard => !s.keys.lock().is_empty(),
            InputKind::Pointer => !s.pointer.lock().is_empty(),
            InputKind::Mice => !s.mice.lock().is_empty(),
        }
}

/// Read whole events (or PS/2 bytes). `None` when nothing is queued.
pub fn read_input(s: &Screen, k: InputKind, buf: &mut [u8]) -> Option<usize> {
    if k == InputKind::Mice {
        let mut q = s.mice.lock();
        if q.is_empty() {
            return None;
        }
        let n = buf.len().min(q.len());
        for (d, v) in buf.iter_mut().zip(q.drain(..n)) {
            *d = v;
        }
        return Some(n);
    }
    let q = if k == InputKind::Keyboard { &s.keys } else { &s.pointer };
    let mut q = q.lock();
    if q.is_empty() {
        return None;
    }
    let mut n = 0;
    while n + 24 <= buf.len() {
        let Some(e) = q.pop_front() else { break };
        buf[n..n + 24].copy_from_slice(&e);
        n += 24;
    }
    Some(n)
}

/// fb_var_screeninfo for the current mode.
pub fn var_info(s: &Screen) -> [u8; 160] {
    let (w, h) = s.size();
    let mut b = [0u8; 160];
    let mut put = |off: usize, v: u32| b[off..off + 4].copy_from_slice(&v.to_le_bytes());
    put(0, w);
    put(4, h);
    put(8, w);
    put(12, h);
    put(24, 32); // bits_per_pixel
    // red, green, blue, transp: (offset, length, msb_right)
    put(32, 16);
    put(36, 8);
    put(44, 8);
    put(48, 8);
    put(56, 0);
    put(60, 8);
    put(68, 24);
    put(72, 0);
    put(88, u32::MAX); // height in mm: unknown
    put(92, u32::MAX);
    put(100, 15_000); // pixclock, ps
    b
}

/// fb_fix_screeninfo for the current mode.
pub fn fix_info(s: &Screen) -> [u8; 80] {
    let b = s.buf.lock().clone();
    let mut f = [0u8; 80];
    f[..8].copy_from_slice(b"MayOS FB");
    f[16..24].copy_from_slice(&b.phys.to_le_bytes());
    f[24..28].copy_from_slice(&(b.w * b.h * 4).to_le_bytes());
    f[36..40].copy_from_slice(&2u32.to_le_bytes()); // FB_VISUAL_TRUECOLOR
    f[42..44].copy_from_slice(&1u16.to_le_bytes()); // ypanstep
    f[48..52].copy_from_slice(&(b.w * 4).to_le_bytes());
    f
}
