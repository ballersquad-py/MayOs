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
    let mut r = RING.lock();
    let _ = RingWriter(&mut r).write_fmt(args);
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
