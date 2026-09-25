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
