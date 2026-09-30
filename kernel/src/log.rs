//! Kernel log: goes to the serial port and to an in-memory ring buffer that
//! the `dmesg` shell command can show.

use alloc::string::String;
use core::fmt::{self, Write};

use crate::sync::Spin;

const CAP: usize = 64 * 1024;

struct Ring {
    buf: [u8; CAP],
    len: usize,
    start: usize,
}

static RING: Spin<Ring> = Spin::new(Ring { buf: [0; CAP], len: 0, start: 0 });

struct RingWriter<'a>(&'a mut Ring);

impl Write for RingWriter<'_> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for &b in s.as_bytes() {
            let r = &mut *self.0;
            let idx = (r.start + r.len) % CAP;
            r.buf[idx] = b;
            if r.len < CAP {
                r.len += 1;
            } else {
                r.start = (r.start + 1) % CAP;
            }
        }
        Ok(())
    }
}

pub fn log_fmt(args: fmt::Arguments) {
    crate::serial::write_fmt(args);
    {
        let mut r = RING.lock();
        let _ = RingWriter(&mut r).write_fmt(args);
    }
    if SCREEN.load(core::sync::atomic::Ordering::Relaxed) {
        screen_line(args);
    }
}

/// Boot messages are shown on the screen until the desktop starts (so a
/// PC that stops while starting shows where).
static SCREEN: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(true);
static SCREEN_Y: Spin<i32> = Spin::new(0);

pub fn stop_screen() {
    SCREEN.store(false, core::sync::atomic::Ordering::Relaxed);
}

fn screen_line(args: fmt::Arguments) {
    struct Buf([u8; 200], usize);
    impl Write for Buf {
        fn write_str(&mut self, s: &str) -> fmt::Result {
            for &b in s.as_bytes() {
                if self.1 < self.0.len() && b != b'\n' {
                    self.0[self.1] = b;
                    self.1 += 1;
                }
            }
            Ok(())
        }
    }
    let Some(fonts) = crate::gui::theme::try_fonts() else { return };
    let Some(fb) = crate::boot::FRAMEBUFFER.response().and_then(|r| r.first()) else { return };
    if fb.bpp != 32 {
        return;
    }
    let Some(mut y) = SCREEN_Y.try_lock() else { return };
    let mut b = Buf([0; 200], 0);
    let _ = b.write_fmt(args);
    let text = core::str::from_utf8(&b.0[..b.1]).unwrap_or("");
    let (w, h) = (fb.width as i32, fb.height as i32);
    let stride = fb.pitch as usize / 4;
    let buf = unsafe { core::slice::from_raw_parts_mut(fb.address as *mut u32, stride * h as usize) };
    let mut c = gfx::Canvas::new(buf, w, h, stride);
    if *y == 0 || *y + 18 > h - 10 {
        c.fill_rect(gfx::Rect::new(0, 0, w, h), gfx::rgb(0x10, 0x12, 0x1a));
        c.draw_text(&fonts.bold, 16, 26, "MayOS is starting (the last line shows where it is)", gfx::rgb(0x9c, 0xc4, 0xff));
        *y = 50;
    }
    c.draw_text(&fonts.mono, 16, *y, text, gfx::rgb(0xe0, 0xe4, 0xec));
    *y += 18;
}

pub fn contents() -> String {
    let r = RING.lock();
    let mut out = alloc::vec::Vec::with_capacity(r.len);
    for i in 0..r.len {
        out.push(r.buf[(r.start + i) % CAP]);
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[macro_export]
macro_rules! kprint {
    ($($arg:tt)*) => { $crate::log::log_fmt(format_args!($($arg)*)) };
}

#[macro_export]
macro_rules! kprintln {
    () => { $crate::kprint!("\n") };
    ($($arg:tt)*) => { $crate::log::log_fmt(format_args!("{}\n", format_args!($($arg)*))) };
}

/// The last `n` lines of the log, without allocating (safe in a panic).
pub fn tail(n: usize) -> TailBuf {
    let mut out = TailBuf { buf: [0; 1024], len: 0 };
    // The panic path must not block: skip the log if it is locked.
    let r = unsafe { &*RING.data_ptr() };
    let mut lines = 0;
    let mut start = r.len;
    while start > 0 {
        let b = r.buf[(r.start + start - 1) % CAP];
        if b == b'\n' && start != r.len {
            lines += 1;
            if lines == n {
                break;
            }
        }
        start -= 1;
    }
    for i in start..r.len {
        if out.len < out.buf.len() {
            out.buf[out.len] = r.buf[(r.start + i) % CAP];
            out.len += 1;
        }
    }
    out
}

pub struct TailBuf {
    buf: [u8; 1024],
    len: usize,
}

impl TailBuf {
    pub fn lines(&self) -> core::str::Lines<'_> {
        core::str::from_utf8(&self.buf[..self.len]).unwrap_or("").lines()
    }
}
